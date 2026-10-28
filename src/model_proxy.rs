//! A run-private Responses translation bridge.
//!
//! Codex 0.164.0 declares a program's MCP tools inside a Responses
//! `{"type":"namespace", ...}` item. OpenAI's own endpoint understands that
//! extension; an endpoint that implements only the plain function-calling
//! protocol does not, so the nested tools are not callable there. This bridge
//! sits between the program and the connection the person configured: it sends
//! each nested function as a plain top-level function to the upstream endpoint
//! and restores the namespace on the reply, so the program keeps its internal
//! tool identity while the connection sees only plain function calls.
//!
//! It is a fixed-target loopback transport, like the run's Git transport: it
//! accepts only this run's model traffic, forwards it to the one configured
//! endpoint, and holds no model key of its own.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The largest request or reply body this bridge will carry.
const MAX_BODY: usize = 32 * 1024 * 1024;

/// How long the upstream may stay silent before the bridge gives up.
///
/// This resets whenever a body byte arrives, so a long but live stream is not
/// cut off; the run's session timeout is the separate total bound.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(110);

/// The canonical flat name for one namespaced tool: `<namespace>__<name>`.
///
/// This is the name the other native programs already see for the same MCP
/// tool, so a flat Forgejo call is the same call everywhere.
pub fn flat_name(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_owned()
    } else {
        format!("{namespace}__{name}")
    }
}

/// What kind of tool one flat function stands for on Codex's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// A plain Responses function call.
    Function,
    /// A Responses custom (freeform) tool call.
    Custom,
}

/// One tool a request declared, keyed by its flat function name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatTool {
    /// The namespace Codex will route the call through.
    pub namespace: String,
    /// The bare tool name inside that namespace.
    pub name: String,
    /// Whether Codex expects a function call or a custom tool call.
    pub kind: ToolKind,
}

/// The tools a request declared, keyed by their flat function name.
///
/// A response call is rewritten only through this exact map, so a model cannot
/// turn an arbitrary flat string into a privileged namespace by guessing a
/// separator.
pub type FlatNames = BTreeMap<String, FlatTool>;

fn register(names: &mut FlatNames, namespace: &str, name: &str, kind: ToolKind) -> String {
    let flat = flat_name(namespace, name);
    names.insert(
        flat.clone(),
        FlatTool {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            kind,
        },
    );
    flat
}

/// Turn one Responses custom (freeform) tool into a plain function with one
/// string input, and keep the original identity in the map.
fn plain_custom_tool(tool: &Value, flat: &str) -> Value {
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "type": "function",
        "name": flat,
        "description": format!(
            "{description}\n\nThe tool's input is one string; pass it as `input`."
        ),
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The tool input exactly as the custom tool defines it."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }
    })
}

/// Move Responses Lite `additional_tools` declarations into the plain `tools`
/// array. A model pinned to direct tools does not use this shape, but a catalog
/// that could not be written must not leave the tools invisible.
fn extract_additional_tools(body: &mut Value) {
    let declared = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if declared {
        return;
    }
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut tools = Vec::new();
    let mut retained = Vec::new();
    for item in std::mem::take(input) {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            if let Some(list) = item.get("tools").and_then(Value::as_array) {
                tools.extend(list.iter().cloned());
            }
        } else {
            retained.push(item);
        }
    }
    *input = retained;
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
}

/// The canonical flat names of one request's tools and history calls.
pub fn flatten_request(body: &mut Value) -> FlatNames {
    extract_additional_tools(body);
    let mut names = FlatNames::new();
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let mut flat = Vec::new();
        for tool in std::mem::take(tools) {
            flatten_tool(tool, &mut names, &mut flat);
        }
        *tools = flat;
    }
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            flatten_history_item(item, &mut names);
        }
    }
    names
}

fn flatten_tool(tool: Value, names: &mut FlatNames, flat: &mut Vec<Value>) {
    match tool.get("type").and_then(Value::as_str) {
        Some("namespace") => {
            let namespace = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(nested) = tool.get("tools").and_then(Value::as_array) else {
                return;
            };
            for function in nested {
                match function.get("type").and_then(Value::as_str) {
                    Some("function") => {
                        let Some(name) = function.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let flat_name = register(names, &namespace, name, ToolKind::Function);
                        let mut function = function.clone();
                        function["name"] = Value::String(flat_name);
                        flat.push(function);
                    }
                    Some("custom") => {
                        let Some(name) = function.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let flat_name = register(names, &namespace, name, ToolKind::Custom);
                        flat.push(plain_custom_tool(function, &flat_name));
                    }
                    // A web-search or search tool has no plain function shape.
                    _ => {}
                }
            }
        }
        Some("function") => flat.push(tool),
        Some("custom") => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                return;
            };
            let flat_name = register(names, "", name, ToolKind::Custom);
            flat.push(plain_custom_tool(&tool, &flat_name));
        }
        // A web-search or search tool has no plain function shape.
        _ => {}
    }
}

/// Flatten one history call, and remember a tool the current request no longer
/// declares so its reply still names the right runtime.
fn flatten_history_item(item: &mut Value, names: &mut FlatNames) {
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    match kind {
        "function_call" => {
            let Some(namespace) = item
                .get("namespace")
                .and_then(Value::as_str)
                .filter(|namespace| !namespace.is_empty())
            else {
                return;
            };
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                return;
            };
            let flat_name = register(names, namespace, name, ToolKind::Function);
            item["name"] = Value::String(flat_name);
            if let Some(object) = item.as_object_mut() {
                object.remove("namespace");
            }
        }
        "custom_tool_call" => {
            let namespace = item
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                return;
            };
            let flat_name = register(names, &namespace, name, ToolKind::Custom);
            let input = item
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            if let Some(object) = item.as_object_mut() {
                object.insert("type".to_owned(), Value::String("function_call".to_owned()));
                object.insert("name".to_owned(), Value::String(flat_name));
                object.insert(
                    "arguments".to_owned(),
                    Value::String(json!({"input": input}).to_string()),
                );
                object.remove("namespace");
                object.remove("input");
            }
        }
        "custom_tool_call_output" => {
            if let Some(object) = item.as_object_mut() {
                object.insert(
                    "type".to_owned(),
                    Value::String("function_call_output".to_owned()),
                );
                object.remove("namespace");
                // The plain function output shape has no tool name.
                object.remove("name");
            }
        }
        _ => {}
    }
}

/// Restore the namespace or custom call shape on every plain call in a reply.
pub fn rewrite_reply(value: &mut Value, names: &FlatNames) {
    if let Some(output) = value.get_mut("output").and_then(Value::as_array_mut) {
        for item in output {
            rewrite_item(item, names);
        }
    }
    // A non-streamed reply is the `response` object; keep the same handling for a
    // caller that wrapped it in an event.
    if let Some(response) = value.get_mut("response") {
        rewrite_reply(response, names);
    }
    if let Some(item) = value.get_mut("item") {
        rewrite_item(item, names);
    }
}

/// Restore the namespace or custom call shape on one output item.
fn rewrite_item(item: &mut Value, names: &FlatNames) {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    if item
        .get("namespace")
        .and_then(Value::as_str)
        .is_some_and(|namespace| !namespace.is_empty())
    {
        return;
    }
    let Some(flat_name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(tool) = names.get(flat_name) else {
        return;
    };
    match tool.kind {
        ToolKind::Function => {
            item["name"] = Value::String(tool.name.clone());
            item["namespace"] = Value::String(tool.namespace.clone());
        }
        ToolKind::Custom => {
            let input = custom_input(item.get("arguments"));
            let call_id = item.get("call_id").cloned();
            let id = item.get("id").cloned();
            let mut replaced = json!({
                "type": "custom_tool_call",
                "name": tool.name,
                "input": input
            });
            if !tool.namespace.is_empty() {
                replaced["namespace"] = Value::String(tool.namespace.clone());
            }
            if let Some(call_id) = call_id {
                replaced["call_id"] = call_id;
            }
            if let Some(id) = id {
                replaced["id"] = id;
            }
            *item = replaced;
        }
    }
}

/// The one string a custom tool call carries, from the function's `input`
/// argument.
fn custom_input(arguments: Option<&Value>) -> String {
    match arguments {
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|value| {
                value
                    .get("input")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| raw.clone()),
        Some(value) => value
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
        None => String::new(),
    }
}

/// Restore namespaces in a streamed server-sent event body.
pub fn rewrite_sse(body: &str, names: &FlatNames) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        out.push_str(&rewrite_sse_line(line, names));
    }
    out
}

/// Restore namespaces on one SSE line, preserving its line ending.
fn rewrite_sse_line(line: &str, names: &FlatNames) -> String {
    let ending = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let Some(data) = trimmed.strip_prefix("data:") else {
        return line.to_owned();
    };
    let data = data.trim_start();
    if data.is_empty() || data == "[DONE]" {
        return line.to_owned();
    }
    let Ok(mut event) = serde_json::from_str::<Value>(data) else {
        return line.to_owned();
    };
    rewrite_reply(&mut event, names);
    format!("data: {}{ending}", event)
}

/// The loopback bridge for one run's Responses traffic.
pub struct ModelProxy {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ModelProxy {
    /// Start a bridge to one configured model base address.
    ///
    /// `session_timeout` is the total bound for one request. `idle_timeout`
    /// resets on every body byte received, so a continuous stream can outlive
    /// it while a silent connection is cut off.
    pub fn start(
        upstream_base: &str,
        session_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let base = upstream_base.trim().trim_end_matches('/');
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err("the model address is not a usable HTTP address".to_owned());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("the run's Responses bridge cannot listen: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("the run's Responses bridge cannot poll: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("the run's Responses bridge has no address: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let upstream = base.to_owned();
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let upstream = upstream.clone();
                        // One connection at a time is normal; a connection owns its
                        // own stream so dropping the run never waits on a silent
                        // upstream body.
                        std::thread::spawn(move || {
                            let _ = serve(stream, &upstream, session_timeout, idle_timeout);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            stop,
            handle: Some(handle),
        })
    }

    /// The base URL the program should use for this run.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for ModelProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One parsed HTTP request.
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn serve(
    mut stream: TcpStream,
    upstream: &str,
    session_timeout: Duration,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let Some(request) = read_request(&mut stream)? else {
        return Ok(());
    };
    let target = format!("{upstream}{}", request.path);
    let (body, names) = if is_responses_path(&request.path) {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(mut value) => {
                let names = flatten_request(&mut value);
                (serde_json::to_vec(&value).unwrap_or(request.body), names)
            }
            Err(_) => (request.body, FlatNames::new()),
        }
    } else {
        (request.body, FlatNames::new())
    };
    let upstream = match forward(
        &request.method,
        &target,
        &request.headers,
        body,
        session_timeout,
    ) {
        Ok(upstream) => upstream,
        Err(error) => {
            let message = format!(
                "the model connection could not be reached through the run's Responses bridge: {error}"
            );
            return write_bridge_error(&mut stream, 502, &message);
        }
    };
    let content_type = upstream
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.to_ascii_lowercase())
        .unwrap_or_default();
    if content_type.contains("text/event-stream") {
        return relay_sse(&mut stream, upstream, &names, idle_timeout, session_timeout);
    }
    let mut reply = match read_body(upstream.body, idle_timeout, session_timeout) {
        Ok(body) => body,
        Err(error) => {
            let (status, message) = body_error(&error);
            return write_bridge_error(&mut stream, status, &message);
        }
    };
    if content_type.contains("application/json")
        && let Ok(mut value) = serde_json::from_slice::<Value>(&reply)
    {
        rewrite_reply(&mut value, &names);
        reply = serde_json::to_vec(&value).unwrap_or(reply);
    }
    write_reply(
        &mut stream,
        upstream.status,
        status_text(upstream.status),
        &upstream.headers,
        &reply,
    )
}

fn body_error(error: &BodyError) -> (u16, String) {
    match error {
        BodyError::Idle => (
            504,
            "the model stopped sending through the run's Responses bridge".to_owned(),
        ),
        BodyError::Total => (
            504,
            "the model connection reached the run's time limit".to_owned(),
        ),
        BodyError::Upstream(error) => (
            502,
            format!("the model connection failed through the run's Responses bridge: {error}"),
        ),
    }
}

fn write_bridge_error(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let body = json!({"error": {"message": message}}).to_string();
    write_reply(
        stream,
        status,
        status_text(status),
        &[("content-type".to_owned(), "application/json".to_owned())],
        body.as_bytes(),
    )
}

/// One upstream response whose body has not been read yet.
struct Upstream {
    status: u16,
    headers: Vec<(String, String)>,
    body: ureq::Body,
}

/// Why a body read stopped before the upstream closed it.
enum BodyError {
    Idle,
    Total,
    Upstream(String),
}

/// The upstream body, delivered in arrival-sized chunks.
struct BodyStream {
    receiver: Receiver<Result<Vec<u8>, String>>,
    handle: Option<JoinHandle<()>>,
    deadline: Instant,
    idle: Duration,
}

impl BodyStream {
    fn new(body: ureq::Body, idle: Duration, session_timeout: Duration) -> Self {
        let (sender, receiver) = mpsc::sync_channel(4);
        let handle = std::thread::spawn(move || {
            let mut body = body.into_reader();
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match body.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender.send(Ok(buffer[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        });
        Self {
            receiver,
            handle: Some(handle),
            deadline: Instant::now() + session_timeout,
            idle,
        }
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, BodyError> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BodyError::Total);
        }
        match self.receiver.recv_timeout(self.idle.min(remaining)) {
            Ok(Ok(chunk)) => Ok(Some(chunk)),
            Ok(Err(error)) => Err(BodyError::Upstream(error)),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= self.deadline {
                    Err(BodyError::Total)
                } else {
                    Err(BodyError::Idle)
                }
            }
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    fn finish(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_body(
    body: ureq::Body,
    idle: Duration,
    session_timeout: Duration,
) -> Result<Vec<u8>, BodyError> {
    let mut stream = BodyStream::new(body, idle, session_timeout);
    let mut reply = Vec::new();
    while let Some(chunk) = stream.next()? {
        if reply.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(BodyError::Upstream("the reply is too large".to_owned()));
        }
        reply.extend_from_slice(&chunk);
    }
    stream.finish();
    Ok(reply)
}

fn relay_sse(
    stream: &mut TcpStream,
    upstream: Upstream,
    names: &FlatNames,
    idle: Duration,
    session_timeout: Duration,
) -> std::io::Result<()> {
    let mut body = BodyStream::new(upstream.body, idle, session_timeout);
    let mut pending = Vec::new();
    let mut started = false;
    loop {
        match body.next() {
            Ok(Some(chunk)) => {
                if !started {
                    write_stream_head(
                        stream,
                        upstream.status,
                        status_text(upstream.status),
                        &upstream.headers,
                    )?;
                    started = true;
                }
                pending.extend_from_slice(&chunk);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line);
                    stream.write_all(rewrite_sse_line(&line, names).as_bytes())?;
                }
                stream.flush()?;
            }
            Ok(None) => {
                if !started {
                    write_stream_head(
                        stream,
                        upstream.status,
                        status_text(upstream.status),
                        &upstream.headers,
                    )?;
                }
                if !pending.is_empty() {
                    let line = String::from_utf8_lossy(&pending);
                    stream.write_all(rewrite_sse_line(&line, names).as_bytes())?;
                }
                stream.flush()?;
                body.finish();
                return Ok(());
            }
            Err(error) => {
                if !started {
                    let (status, message) = body_error(&error);
                    return write_bridge_error(stream, status, &message);
                }
                // The status and some bytes are already on the wire; closing the
                // connection is the only truthful signal left.
                return Ok(());
            }
        }
    }
}

fn write_stream_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        if forward_reply_header(name) {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

fn is_responses_path(path: &str) -> bool {
    path.split('?')
        .next()
        .is_some_and(|path| path.trim_end_matches('/').ends_with("/responses"))
}

fn forward(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    session_timeout: Duration,
) -> Result<Upstream, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(session_timeout))
        .timeout_connect(Some(session_timeout.min(Duration::from_secs(30))))
        .timeout_send_request(Some(session_timeout))
        .timeout_send_body(Some(session_timeout))
        .timeout_recv_response(Some(session_timeout))
        .timeout_recv_body(Some(session_timeout))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = if method.eq_ignore_ascii_case("GET") {
        let mut request = agent.get(target);
        for (name, value) in headers {
            if forward_request_header(name) {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        request
            .header("accept-encoding", "identity")
            .call()
            .map_err(|error| error.to_string())?
    } else {
        let mut request = agent.post(target);
        for (name, value) in headers {
            if forward_request_header(name) {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        // Keep the upstream body readable without a second decoder, so a rewritten
        // stream is the same bytes the endpoint sent.
        request
            .header("accept-encoding", "identity")
            .send(body)
            .map_err(|error| error.to_string())?
    };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    Ok(Upstream {
        status,
        headers,
        body: response.into_body(),
    })
}

fn forward_request_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "accept-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
    )
}

fn forward_reply_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "content-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "upgrade"
    )
}

fn write_reply(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        if forward_reply_header(name) {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let Some(method) = parts.next().map(str::to_owned) else {
        return Ok(None);
    };
    let Some(target) = parts.next().map(str::to_owned) else {
        return Ok(None);
    };
    let path = target
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|at| rest[at..].to_owned()))
        .unwrap_or_else(|| target.to_owned());
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
        if headers.len() > 200 {
            return Ok(None);
        }
    }
    let body = if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|piece| piece.trim().eq_ignore_ascii_case("chunked"))
    }) {
        read_chunked(&mut reader)?
    } else {
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        if length > MAX_BODY {
            return Ok(None);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn read_chunked(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let size = line
            .trim()
            .split(';')
            .next()
            .and_then(|size| usize::from_str_radix(size, 16).ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size"))?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 {
                    break;
                }
                if trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if body.len().saturating_add(size) > MAX_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "body too large",
            ));
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut end = [0u8; 2];
        reader.read_exact(&mut end)?;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_request() -> Value {
        json!({
            "model": "lab-model",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "list my issues"}
                ]},
                {
                    "type": "function_call",
                    "name": "forgejo_list_assigned_issues",
                    "namespace": "mcp__gewu_forgejo",
                    "arguments": "{}",
                    "call_id": "call_1"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "name": "exec_command",
                    "description": "run a command",
                    "strict": false,
                    "parameters": {"type": "object", "properties": {}}
                },
                {
                    "type": "namespace",
                    "name": "mcp__gewu_forgejo",
                    "description": "Tools in the mcp__gewu_forgejo namespace.",
                    "tools": [{
                        "type": "function",
                        "name": "forgejo_list_assigned_issues",
                        "description": "list assigned issues",
                        "strict": false,
                        "parameters": {"type": "object", "properties": {}}
                    }]
                },
                {"type": "web_search"}
            ]
        })
    }

    #[test]
    fn a_namespace_becomes_a_plain_function_and_history_uses_the_same_name() {
        let mut body = codex_request();
        let names = flatten_request(&mut body);
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(
            tools.len(),
            2,
            "the web search tool has no plain shape: {tools:?}"
        );
        assert_eq!(tools[0]["name"], "exec_command");
        assert_eq!(
            tools[1]["name"],
            "mcp__gewu_forgejo__forgejo_list_assigned_issues"
        );
        assert_eq!(tools[1]["type"], "function");
        assert!(
            !serde_json::to_string(&body)
                .expect("json")
                .contains("\"namespace\"")
        );
        assert_eq!(
            body["input"][1]["name"],
            "mcp__gewu_forgejo__forgejo_list_assigned_issues"
        );
        assert_eq!(
            names
                .get("mcp__gewu_forgejo__forgejo_list_assigned_issues")
                .map(|tool| (tool.namespace.as_str(), tool.name.as_str())),
            Some(("mcp__gewu_forgejo", "forgejo_list_assigned_issues"))
        );
    }

    #[test]
    fn a_plain_call_is_restored_to_the_namespace_codex_expects() {
        let mut request = codex_request();
        let names = flatten_request(&mut request);
        let reply = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "name": "mcp__gewu_forgejo__forgejo_list_assigned_issues",
                    "arguments": "{}",
                    "call_id": "call_1"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "output": [{
                    "type": "function_call",
                    "name": "mcp__gewu_forgejo__forgejo_list_assigned_issues",
                    "arguments": "{}",
                    "call_id": "call_1"
                }]}
            })
        );
        let rewritten = rewrite_sse(&reply, &names);
        assert!(rewritten.contains("\"namespace\":\"mcp__gewu_forgejo\""));
        assert!(rewritten.contains("\"name\":\"forgejo_list_assigned_issues\""));
        assert!(!rewritten.contains("mcp__gewu_forgejo__forgejo_list_assigned_issues"));
        assert!(rewritten.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn a_namespace_call_from_an_endpoint_that_understands_it_is_left_alone() {
        let mut request = codex_request();
        let names = flatten_request(&mut request);
        let mut reply = json!({
            "output": [{
                "type": "function_call",
                "name": "forgejo_list_assigned_issues",
                "namespace": "mcp__gewu_forgejo",
                "arguments": "{}",
                "call_id": "call_1"
            }]
        });
        rewrite_reply(&mut reply, &names);
        assert_eq!(reply["output"][0]["name"], "forgejo_list_assigned_issues");
        assert_eq!(reply["output"][0]["namespace"], "mcp__gewu_forgejo");
    }

    #[test]
    fn a_flat_name_that_the_request_did_not_declare_is_not_invented() {
        let mut request = codex_request();
        let names = flatten_request(&mut request);
        let mut reply = json!({
            "output": [{
                "type": "function_call",
                "name": "mcp__other__secret",
                "arguments": "{}",
                "call_id": "call_1"
            }]
        });
        rewrite_reply(&mut reply, &names);
        assert_eq!(reply["output"][0]["name"], "mcp__other__secret");
        assert!(reply["output"][0].get("namespace").is_none());
    }

    #[test]
    fn a_custom_tool_becomes_a_one_string_function() {
        let mut body = json!({
            "tools": [
                {"type": "custom", "name": "apply_patch", "description": "Edit files."},
                {"type": "function", "name": "exec_command"},
                {"type": "namespace", "name": "ns", "tools": [
                    {"type": "custom", "name": "custom_nested", "description": "Nested."}
                ]}
            ]
        });
        let names = flatten_request(&mut body);
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["name"], "apply_patch");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(
            tools[0]["parameters"]["properties"]["input"]["type"],
            "string"
        );
        assert_eq!(tools[2]["name"], "ns__custom_nested");
        assert_eq!(names["apply_patch"].kind, ToolKind::Custom);
        assert_eq!(names["ns__custom_nested"].kind, ToolKind::Custom);
    }

    #[test]
    fn a_plain_call_to_a_custom_tool_becomes_a_custom_tool_call() {
        let mut request = json!({
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Edit files."
            }]
        });
        let names = flatten_request(&mut request);
        let mut reply = json!({
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
            }]
        });
        rewrite_reply(&mut reply, &names);
        assert_eq!(reply["output"][0]["type"], "custom_tool_call");
        assert_eq!(reply["output"][0]["name"], "apply_patch");
        assert_eq!(
            reply["output"][0]["input"],
            "*** Begin Patch\n*** End Patch"
        );
        assert_eq!(reply["output"][0]["call_id"], "call_1");
    }

    #[test]
    fn responses_lite_additional_tools_become_plain_functions() {
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "work"},
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "functions",
                        "tools": [{
                            "type": "custom",
                            "name": "exec",
                            "description": "Run JavaScript."
                        }, {
                            "type": "function",
                            "name": "wait"
                        }]
                    }]
                }
            ]
        });
        let names = flatten_request(&mut body);
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "functions__exec");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[1]["name"], "functions__wait");
        assert_eq!(body["input"].as_array().expect("input").len(), 1);
        assert_eq!(names["functions__exec"].kind, ToolKind::Custom);
    }

    #[test]
    fn a_custom_history_call_becomes_a_plain_function_call() {
        let mut request = json!({
            "input": [{
                "type": "custom_tool_call",
                "name": "apply_patch",
                "input": "*** Begin Patch",
                "call_id": "call_1"
            }, {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "done"
            }]
        });
        let names = flatten_request(&mut request);
        assert_eq!(request["input"][0]["type"], "function_call");
        assert_eq!(
            request["input"][0]["arguments"],
            "{\"input\":\"*** Begin Patch\"}"
        );
        assert_eq!(request["input"][1]["type"], "function_call_output");
        assert_eq!(names["apply_patch"].kind, ToolKind::Custom);
    }
}
