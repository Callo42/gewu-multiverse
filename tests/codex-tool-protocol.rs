//! The exact Responses shapes Codex 0.164.0 sends and receives, driven through the
//! product's run-private bridge.
//!
//! One server here implements only the plain function-calling protocol. It rejects
//! a `type: "namespace"` tool outright, so a run that still sends Codex's extension
//! cannot pass. The other server accepts the plain shapes and answers in the
//! namespace shape, proving the bridge leaves an endpoint that understands the
//! extension alone.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gewu_multiverse::model_proxy::ModelProxy;
use serde_json::{Value, json};

const TOOL: &str = "mcp__gewu_forgejo__forgejo_list_assigned_issues";

/// A loopback Responses endpoint with a caller-supplied handler.
struct Endpoint {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Endpoint {
    fn start(handler: impl Fn(&Value) -> Result<String, String> + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the endpoint binds");
        let address = listener.local_addr().expect("the endpoint address");
        listener.set_nonblocking(true).expect("the endpoint polls");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let seen_thread = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve(stream, &handler, &seen_thread);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn body(&self) -> Value {
        self.seen
            .lock()
            .expect("seen")
            .first()
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(50));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A loopback Responses endpoint that writes its own stream.
struct StreamEndpoint {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl StreamEndpoint {
    fn start(handler: impl Fn(&mut TcpStream, &Value) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the stream endpoint binds");
        let address = listener.local_addr().expect("the stream endpoint address");
        listener
            .set_nonblocking(true)
            .expect("the stream endpoint polls");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handler = Arc::new(handler);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let handler = Arc::clone(&handler);
                        thread::spawn(move || {
                            if let Ok(request) = read_http_request(&mut stream) {
                                handler(&mut stream, &request);
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for StreamEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(50));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn write_stream_head(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    )?;
    stream.flush()
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Value> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let _method = parts.next();
    let _path = parts.next();
    let mut length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn serve(
    mut stream: TcpStream,
    handler: &(impl Fn(&Value) -> Result<String, String> + Send),
    seen: &Mutex<Vec<Value>>,
) -> std::io::Result<()> {
    let request = read_http_request(&mut stream)?;
    seen.lock().expect("seen").push(request.clone());
    let reply = match handler(&request) {
        Ok(body) => (200, body),
        Err(error) => (400, json!({"error": {"message": error}}).to_string()),
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.0,
        if reply.0 == 200 { "OK" } else { "Bad Request" },
        reply.1.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(reply.1.as_bytes())?;
    stream.flush()
}

fn sse(items: &[Value]) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "data: {}\n\n",
        json!({"type": "response.created", "response": {"id": "resp_1"}})
    ));
    for item in items {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"type": "response.output_item.done", "item": item})
        ));
    }
    body.push_str(&format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "output": items}
        })
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

fn codex_request() -> Value {
    json!({
        "model": "lab-model",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "list my assigned issues"}]
            },
            {
                "type": "function_call",
                "name": "forgejo_list_assigned_issues",
                "namespace": "mcp__gewu_forgejo",
                "arguments": "{}",
                "call_id": "call_previous"
            },
            {
                "type": "function_call_output",
                "call_id": "call_previous",
                "output": "no issues"
            }
        ],
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Run a command.",
                "strict": false,
                "parameters": {"type": "object", "properties": {}}
            },
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Edit files in the workspace.",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}
            },
            {
                "type": "namespace",
                "name": "mcp__gewu_forgejo",
                "description": "Tools in the mcp__gewu_forgejo namespace.",
                "tools": [{
                    "type": "function",
                    "name": "forgejo_list_assigned_issues",
                    "description": "List the open issues assigned to the agent.",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false
                    }
                }]
            },
            {"type": "web_search"}
        ]
    })
}

fn post(base: &str, body: &Value) -> (u16, String) {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .post(&format!("{base}/responses"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer gw-synthetic-key")
        .send(body.to_string())
        .expect("the bridge answers");
    let status = response.status().as_u16();
    (
        status,
        response
            .into_body()
            .read_to_string()
            .expect("the reply body"),
    )
}

/// Send one request and return the status, the time to the first body byte, and
/// the whole body.
fn post_with_first_byte(base: &str, body: &Value) -> (u16, Duration, String) {
    let start = Instant::now();
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .post(&format!("{base}/responses"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer gw-synthetic-key")
        .send(body.to_string())
        .expect("the bridge answers");
    let status = response.status().as_u16();
    let mut reader = response.into_body().into_reader();
    let mut first = [0u8; 1];
    let read = reader.read(&mut first).expect("the first body byte");
    let first_at = start.elapsed();
    let mut rest = Vec::new();
    if read > 0 {
        reader.read_to_end(&mut rest).expect("the rest of the body");
    }
    let mut body = Vec::new();
    if read > 0 {
        body.push(first[0]);
    }
    body.extend_from_slice(&rest);
    (
        status,
        first_at,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[test]
fn the_bridge_relays_a_slow_continuous_stream_as_it_arrives() {
    let endpoint = StreamEndpoint::start(|stream, _request| {
        let _ = write_stream_head(stream);
        for index in 0..15 {
            let item = if index == 0 {
                json!({
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": TOOL,
                    "arguments": "{}"
                })
            } else {
                json!({
                    "type": "message",
                    "id": format!("msg_{index}"),
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "working"}]
                })
            };
            let _ = write!(
                stream,
                "event: response.output_item.done\ndata: {}\n\n",
                json!({"type": "response.output_item.done", "item": item})
            );
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(100));
        }
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(5),
        Duration::from_millis(300),
    )
    .expect("proxy");
    let (status, first_at, body) = post_with_first_byte(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{body}");
    assert!(
        first_at < Duration::from_millis(800),
        "the first byte waited for the whole stream: {first_at:?}"
    );
    assert!(body.contains("event: response.output_item.done"), "{body}");
    assert!(
        body.contains("\"namespace\":\"mcp__gewu_forgejo\""),
        "{body}"
    );
    assert!(body.contains("data: [DONE]"), "{body}");
}

#[test]
fn the_bridge_times_out_a_silent_body_after_the_idle_timeout() {
    let endpoint = StreamEndpoint::start(|stream, _request| {
        let _ = write_stream_head(stream);
        thread::sleep(Duration::from_secs(2));
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(5),
        Duration::from_millis(200),
    )
    .expect("proxy");
    let started = Instant::now();
    let (status, body) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 504, "{body}");
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "the idle timeout did not fire: {:?}",
        started.elapsed()
    );
}

#[test]
fn the_bridge_stops_at_the_session_timeout_not_a_body_deadline() {
    let endpoint = StreamEndpoint::start(|stream, _request| {
        let _ = write_stream_head(stream);
        for index in 0..30 {
            let _ = write!(
                stream,
                "data: {}\n\n",
                json!({"type": "response.created", "response": {"id": format!("r{index}")}})
            );
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(100));
        }
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_millis(500),
        Duration::from_millis(250),
    )
    .expect("proxy");
    let started = Instant::now();
    let (status, body) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the session timeout did not bound the stream: {:?}",
        started.elapsed()
    );
    assert!(
        !body.contains("r29"),
        "the stream outlived the session: {body}"
    );
}

#[test]
fn a_namespace_request_reaches_a_plain_endpoint_as_functions() {
    let endpoint = Endpoint::start(|request| {
        let tools = request["tools"].as_array().expect("tools are an array");
        if tools.iter().any(|tool| tool["type"] == "namespace") {
            return Err("a namespace tool reached a plain endpoint".to_owned());
        }
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        if !names.contains(&"exec_command")
            || !names.contains(&"apply_patch")
            || !names.contains(&TOOL)
        {
            return Err(format!("the plain tools are wrong: {names:?}"));
        }
        if names.len() != 3 {
            return Err(format!("an unsupported tool was forwarded: {names:?}"));
        }
        let patch = tools
            .iter()
            .find(|tool| tool["name"] == "apply_patch")
            .expect("apply_patch");
        if patch["type"] != "function"
            || patch["parameters"]["properties"]["input"]["type"] != "string"
        {
            return Err(format!("apply_patch is not a one-string function: {patch}"));
        }
        let history = request["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("the history call");
        if history["name"] != TOOL || history.get("namespace").is_some() {
            return Err(format!("the history call is not plain: {history}"));
        }
        Ok(sse(&[json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": TOOL,
            "arguments": "{}"
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let (status, reply) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{reply}");
    assert!(
        reply.contains("\"namespace\":\"mcp__gewu_forgejo\""),
        "{reply}"
    );
    assert!(
        reply.contains("\"name\":\"forgejo_list_assigned_issues\""),
        "{reply}"
    );
    assert!(!reply.contains(TOOL), "{reply}");
    let seen = endpoint.body();
    assert_eq!(seen["tools"].as_array().expect("tools").len(), 3);
}

#[test]
fn a_plain_call_to_a_custom_tool_is_restored_to_codex() {
    let endpoint = Endpoint::start(|request| {
        let tools = request["tools"].as_array().expect("tools");
        let patch = tools
            .iter()
            .find(|tool| tool["name"] == "apply_patch")
            .expect("apply_patch");
        if patch["type"] != "function" {
            return Err(format!("apply_patch is not a plain function: {patch}"));
        }
        Ok(sse(&[json!({
            "type": "function_call",
            "id": "fc_patch",
            "call_id": "call_patch",
            "name": "apply_patch",
            "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let (status, reply) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{reply}");
    assert!(reply.contains("\"type\":\"custom_tool_call\""), "{reply}");
    assert!(reply.contains("*** Begin Patch"), "{reply}");
    assert!(!reply.contains("apply_patch\",\"arguments"), "{reply}");
}

#[test]
fn an_endpoint_that_understands_namespaces_keeps_its_namespaced_reply() {
    let endpoint = Endpoint::start(|request| {
        let tools = request["tools"].as_array().expect("tools");
        if tools.iter().any(|tool| tool["type"] == "namespace") {
            return Err("the bridge should present flat tools even here".to_owned());
        }
        Ok(sse(&[json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "forgejo_list_assigned_issues",
            "namespace": "mcp__gewu_forgejo",
            "arguments": "{}"
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let (status, reply) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{reply}");
    assert!(
        reply.contains("\"namespace\":\"mcp__gewu_forgejo\""),
        "{reply}"
    );
    assert!(
        reply.contains("\"name\":\"forgejo_list_assigned_issues\""),
        "{reply}"
    );
}

#[test]
fn the_bridge_targets_the_configured_base_address() {
    let endpoint = Endpoint::start(|_| {
        Ok(sse(&[json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}]
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let (status, reply) = post(&proxy.url(), &codex_request());
    assert_eq!(status, 200, "{reply}");
    assert!(reply.contains("ok"), "{reply}");
    // The endpoint's own body is unchanged when the reply has no tool call.
    let seen = endpoint.body();
    assert_eq!(seen["model"], "lab-model");
}

/// A real Codex 0.164.0 process calls its Forgejo tool through the bridge, against
/// an endpoint that implements only plain function calling.
///
/// The official package is not downloaded by the test suite. Point
/// `GEWU_CODEX_BIN` at the package's `bin/codex` and run with `--ignored`.
#[test]
#[ignore = "needs the official Codex 0.164.0 package at GEWU_CODEX_BIN"]
fn the_real_codex_program_calls_a_forgejo_tool_on_a_plain_endpoint() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};
    use std::fs;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    let codex = std::env::var("GEWU_CODEX_BIN").expect("GEWU_CODEX_BIN");
    let codex = PathBuf::from(codex);
    assert!(codex.is_file(), "{} is not a file", codex.display());

    let root = std::env::temp_dir().join(format!(
        "gewu-codex-flat-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let workspace = root.join("workspace");
    let home = root.join("codex-home");
    let context = root.join("mcp-context");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("codex home");
    fs::create_dir_all(&context).expect("mcp context");

    let forgejo = Stub::start_routed(vec![Route::new(
        "GET",
        "/api/v1/repos/issues/search",
        StubReply::json("[]"),
    )]);
    let forgejo_url = format!("http://{}", forgejo.addr());
    fs::write(
        context.join("forgejo.json"),
        json!({
            "url": forgejo_url,
            "login": "gewu-agent",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}]
        })
        .to_string(),
    )
    .expect("forgejo context");
    fs::write(context.join("token"), "gw-synthetic-agent-token").expect("token");
    fs::write(context.join("agent"), "agent-1").expect("agent");
    fs::write(context.join("workspace"), workspace.display().to_string()).expect("workspace file");
    fs::write(context.join("writes"), "0").expect("writes");

    let endpoint = Endpoint::start(|request| {
        let tools = request["tools"].as_array().expect("tools");
        if tools.iter().any(|tool| tool["type"] == "namespace") {
            return Err("the real Codex request kept its namespace wrapper".to_owned());
        }
        if !tools.iter().any(|tool| tool["name"] == TOOL) {
            return Err(format!("the plain Forgejo tool is missing: {tools:?}"));
        }
        if request["input"]
            .as_array()
            .expect("input")
            .iter()
            .filter(|item| item["type"] == "function_call")
            .any(|item| item.get("namespace").is_some())
        {
            return Err("a history call kept its namespace".to_owned());
        }
        if request["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| item["type"] == "function_call_output")
        {
            return Ok(sse(&[json!({
                "type": "message",
                "id": "msg_done",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "the tool was called"}]
            })]));
        }
        Ok(sse(&[json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": TOOL,
            "arguments": "{}"
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let key = root.join("key");
    fs::write(&key, "gw-synthetic-model-key").expect("key");
    fs::write(
        home.join("config.toml"),
        format!(
            "model = \"lab-model\"\nmodel_provider = \"gewu\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\nweb_search = \"disabled\"\n\n[features]\nplugins = false\nrecommended_plugins = false\nmulti_agent = false\n\n[model_providers.gewu]\nname = \"gewu\"\nbase_url = \"{proxy}\"\nwire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\n\n[model_providers.gewu.auth]\ncommand = \"/bin/cat\"\nargs = [\"{}\"]\n\n[mcp_servers.gewu_forgejo]\ncommand = \"{}\"\nargs = [\"--mcp-forgejo\", \"{}\"]\nrequired = true\nstartup_timeout_sec = 20\ntool_timeout_sec = 120\n",
            key.display(),
            env!("CARGO_BIN_EXE_gewu-multiverse"),
            context.display(),
            proxy = proxy.url()
        ),
    )
    .expect("codex config");

    let mut child = std::process::Command::new(&codex)
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("-C")
        .arg(&workspace)
        .arg("--sandbox")
        .arg("danger-full-access")
        .arg("list my assigned issues")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("CODEX_HOME", &home)
        .env("RUST_LOG", "codex_core=warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("codex starts");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("codex status").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the real Codex did not finish within 60 s");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().expect("codex output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("real Codex on a plain endpoint:\n{stdout}\n{stderr}");
    assert!(output.status.success(), "codex failed: {stderr}\n{stdout}");
    assert!(stdout.contains("\"mcp_tool_call\""), "{stdout}\n{stderr}");
    assert!(stdout.contains("\"completed\""), "{stdout}\n{stderr}");
    assert!(stdout.contains("turn.completed"), "{stdout}\n{stderr}");
    assert!(
        !stdout.contains("unsupported call"),
        "Codex could not route its tool: {stdout}\n{stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Write a one-model catalog from Codex's own metadata, with direct tools.
fn direct_catalog(
    codex: &std::path::Path,
    model: &str,
    root: &std::path::Path,
) -> std::path::PathBuf {
    let home = root.join("debug-home");
    std::fs::create_dir_all(&home).expect("debug home");
    let output = std::process::Command::new(codex)
        .arg("debug")
        .arg("models")
        .env_clear()
        .env("HOME", root)
        .env("CODEX_HOME", &home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("codex debug models");
    assert!(output.status.success(), "codex debug models failed");
    let catalog: Value = serde_json::from_slice(&output.stdout).expect("catalog JSON");
    let models = catalog["models"].as_array().expect("models");
    let template = models
        .iter()
        .find(|entry| entry["slug"] == model)
        .or_else(|| {
            models.iter().find(|entry| {
                entry.get("tool_mode").is_none_or(Value::is_null)
                    && entry.get("use_responses_lite") != Some(&Value::Bool(true))
            })
        })
        .or_else(|| models.first())
        .expect("a catalog template");
    let mut entry = template.clone();
    let object = entry.as_object_mut().expect("template object");
    for (key, value) in [
        ("slug", json!(model)),
        ("display_name", json!(model)),
        ("description", json!(model)),
        ("visibility", json!("list")),
        ("shell_type", json!("unified_exec")),
        ("apply_patch_tool_type", json!("freeform")),
        ("web_search_tool_type", json!("text")),
        ("supports_search_tool", json!(false)),
        ("use_responses_lite", json!(false)),
        ("tool_mode", Value::Null),
        ("multi_agent_version", Value::Null),
        ("multi_agent_reasoning_effort", Value::Null),
        ("supports_reasoning_summary_parameter", json!(false)),
        ("default_reasoning_summary", json!("none")),
        ("default_reasoning_level", Value::Null),
        ("supported_reasoning_levels", json!([])),
        ("service_tiers", json!([])),
        ("default_service_tier", Value::Null),
        ("experimental_supported_tools", json!([])),
        ("include_apps_usage_instructions", json!(false)),
        ("include_plugin_usage_instructions", json!(false)),
    ] {
        object.insert(key.to_owned(), value);
    }
    let path = root.join("catalog.json");
    std::fs::write(&path, json!({"models": [entry]}).to_string()).expect("catalog");
    path
}

/// A known model id (which Codex would otherwise run in Responses Lite or code
/// mode) still uses the plain function bridge, including `apply_patch`.
#[test]
#[ignore = "needs the official Codex 0.164.0 package at GEWU_CODEX_BIN"]
fn the_real_codex_program_uses_a_known_model_catalog_and_custom_tool() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::time::UNIX_EPOCH;

    let codex = PathBuf::from(std::env::var("GEWU_CODEX_BIN").expect("GEWU_CODEX_BIN"));
    assert!(codex.is_file(), "{} is not a file", codex.display());
    let root = std::env::temp_dir().join(format!(
        "gewu-codex-known-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let workspace = root.join("workspace");
    let home = root.join("codex-home");
    let context = root.join("mcp-context");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("codex home");
    fs::create_dir_all(&context).expect("mcp context");
    let catalog = direct_catalog(&codex, "gpt-5.6-sol", &root);

    let forgejo = Stub::start_routed(vec![Route::new(
        "GET",
        "/api/v1/repos/issues/search",
        StubReply::json("[]"),
    )]);
    fs::write(
        context.join("forgejo.json"),
        json!({
            "url": format!("http://{}", forgejo.addr()),
            "login": "gewu-agent",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}]
        })
        .to_string(),
    )
    .expect("forgejo context");
    fs::write(context.join("token"), "gw-synthetic-agent-token").expect("token");
    fs::write(context.join("agent"), "agent-1").expect("agent");
    fs::write(context.join("workspace"), workspace.display().to_string()).expect("workspace file");
    fs::write(context.join("writes"), "0").expect("writes");

    let requests = Arc::new(AtomicUsize::new(0));
    let requests_thread = Arc::clone(&requests);
    let endpoint = Endpoint::start(move |request| {
        requests_thread.fetch_add(1, Ordering::Relaxed);
        let output_calls: Vec<&str> = request["input"]
            .as_array()
            .expect("input")
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .filter_map(|item| item["call_id"].as_str())
            .collect();
        if output_calls.contains(&"call_forgejo") {
            return Ok(sse(&[json!({
                "type": "message",
                "id": "msg_done",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "the tools ran"}]
            })]));
        }
        if output_calls.contains(&"call_patch") {
            return Ok(sse(&[json!({
                "type": "function_call",
                "id": "fc_forgejo",
                "call_id": "call_forgejo",
                "name": TOOL,
                "arguments": "{}"
            })]));
        }
        Ok(sse(&[json!({
            "type": "function_call",
            "id": "fc_patch",
            "call_id": "call_patch",
            "name": "apply_patch",
            "arguments": json!({"input": "*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch"}).to_string()
        })]))
    });
    let proxy = ModelProxy::start(
        &endpoint.base_url(),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )
    .expect("proxy");
    let key = root.join("key");
    fs::write(&key, "gw-synthetic-model-key").expect("key");
    fs::write(
        home.join("config.toml"),
        format!(
            "model = \"gpt-5.6-sol\"\nmodel_provider = \"gewu\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\nweb_search = \"disabled\"\nmodel_catalog_json = \"{}\"\n\n[features]\nplugins = false\nrecommended_plugins = false\nmulti_agent = false\n\n[model_providers.gewu]\nname = \"gewu\"\nbase_url = \"{proxy}\"\nwire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\n\n[model_providers.gewu.auth]\ncommand = \"/bin/cat\"\nargs = [\"{}\"]\n\n[mcp_servers.gewu_forgejo]\ncommand = \"{}\"\nargs = [\"--mcp-forgejo\", \"{}\"]\nrequired = true\nstartup_timeout_sec = 20\ntool_timeout_sec = 120\n",
            catalog.display(),
            key.display(),
            env!("CARGO_BIN_EXE_gewu-multiverse"),
            context.display(),
            proxy = proxy.url()
        ),
    )
    .expect("codex config");
    let mut child = std::process::Command::new(&codex)
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("-C")
        .arg(&workspace)
        .arg("--sandbox")
        .arg("danger-full-access")
        .arg("patch hello.txt and list my issues")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("CODEX_HOME", &home)
        .env("RUST_LOG", "codex_core=warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("codex starts");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("codex status").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the real Codex did not finish within 60 s");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().expect("codex output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "codex failed: {stderr}\n{stdout}");
    assert!(
        requests.load(Ordering::Relaxed) >= 3,
        "the patch and Forgejo calls did not both run:\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap_or_default(),
        "hello\n"
    );
    assert!(stdout.contains("\"mcp_tool_call\""), "{stdout}\n{stderr}");
    assert!(stdout.contains("\"completed\""), "{stdout}\n{stderr}");
    let _ = fs::remove_dir_all(&root);
}
