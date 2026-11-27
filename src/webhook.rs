//! Loopback-only Forgejo webhook receiver for event-driven and hybrid role wakes.
//!
//! Multiverse's bundled Forgejo can emit local-only webhook events (`issues`,
//! `issue_comment`, `pull_request`, `pull_request_comment`, `pull_request_approved`,
//! `pull_request_rejected`, `push`, `repository`) to `127.0.0.1:<ephemeral-port>`.
//!
//! Security & scheduling invariants:
//! - Strictly loopback (`127.0.0.1:0`), verified by HMAC-SHA256 (`X-Forgejo-Signature`
//!   / `X-Gitea-Signature` / `X-Hub-Signature-256`) with a per-boot random secret.
//! - Constant-time signature verification; unsigned or mismatched requests get 401.
//! - Anti-echo: the engine never wakes a role for events whose `sender.login` is
//!   that same role's own Forgejo login.
//! - **Stopped-role iron law:** webhook events NEVER wake any role with
//!   `Desired::Stopped` (enforced by the engine).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::forgejo::ForgejoClient;

/// Maximum accepted webhook HTTP request size (256 KiB).
const MAX_WEBHOOK_BYTES: usize = 256 * 1024;
/// Maximum HTTP header block size (16 KiB).
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Absolute per-connection deadline (prevents slowloris stalls on the listener).
const MAX_CONN_DURATION: Duration = Duration::from_secs(5);
/// Maximum concurrent in-flight webhook connections so one slow client can never
/// stall Forgejo deliveries.
const MAX_CONCURRENT_CONNS: usize = 8;
/// Maximum cached delivery deduplication keys.
const MAX_DEDUPE_ENTRIES: usize = 512;
/// TTL for webhook delivery deduplication.
const DEDUPE_TTL: Duration = Duration::from_secs(120);

/// Organization webhook events subscribed on the bundled Forgejo `team` org.
pub const ORG_WEBHOOK_EVENTS: &[&str] = &[
    "issues",
    "issue_assign",
    "issue_label",
    "issue_comment",
    "pull_request",
    "pull_request_assign",
    "pull_request_comment",
    "pull_request_approved",
    "pull_request_rejected",
    "pull_request_sync",
    "push",
    "repository",
];

/// Path prefix used to identify Multiverse-managed local webhooks on the org.
pub const WEBHOOK_PATH: &str = "/gewu-webhook";

/// One verified, parsed event delivered from bundled Forgejo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeEvent {
    pub event: String,
    pub action: String,
    pub actor: String,
    pub repository: String,
    pub number: Option<u64>,
    pub summary: String,
    /// Logins directly involved (assignees, PR author, `@mentions`).
    pub mentioned_logins: Vec<String>,
    /// True when the event involves a pull request (reviewers/maintainers care).
    pub is_pull_request: bool,
    /// True when the event involves an unassigned issue (triage/PM roles care).
    pub is_unassigned_issue: bool,
    pub received_at: Instant,
}

/// Loopback HTTP receiver owned by `Engine`.
pub struct WebhookReceiver {
    addr: SocketAddr,
    secret: String,
    stop: Arc<AtomicBool>,
    rx: Receiver<WakeEvent>,
    thread: Option<JoinHandle<()>>,
}

impl WebhookReceiver {
    /// Bind on `127.0.0.1:0` with a fresh random HMAC secret and spawn the listener thread.
    pub fn start() -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| format!("local webhook listener cannot bind: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("local webhook address cannot be read: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("local webhook listener nonblocking failed: {e}"))?;
        let secret = random_hex_secret()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop_bg = Arc::clone(&stop);
        let secret_bg = Arc::new(secret.clone());
        let thread = std::thread::spawn(move || {
            serve_loop(listener, secret_bg, stop_bg, tx);
        });
        Ok(Self {
            addr,
            secret,
            stop,
            rx,
            thread: Some(thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}{WEBHOOK_PATH}", self.addr)
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Drain all verified webhook events received since the last tick.
    pub fn drain_events(&self) -> Vec<WakeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

impl Drop for WebhookReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(80));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Idempotently register or update this engine's loopback webhook on `org`,
/// removing stale Multiverse loopback hooks pointing to dead ports.
pub fn ensure_org_webhook(
    admin_client: &ForgejoClient,
    organisation: &str,
    url: &str,
    secret: &str,
) -> Result<u64, String> {
    let hooks = admin_client
        .list_org_hooks(organisation)
        .map_err(|e| format!("cannot list Forgejo org webhooks: {e}"))?;
    let mut managed: Vec<_> = hooks
        .into_iter()
        .filter(|h| is_managed_loopback_hook(&h.url))
        .collect();
    if let Some(first) = managed.first().cloned() {
        // Delete any duplicate managed hooks from earlier runs.
        for extra in managed.drain(1..) {
            let _ = admin_client.delete_org_hook(organisation, extra.id);
        }
        let updated = admin_client
            .update_org_hook(organisation, first.id, url, secret, ORG_WEBHOOK_EVENTS)
            .map_err(|e| format!("cannot update Forgejo org webhook: {e}"))?;
        return Ok(updated.id);
    }
    let created = admin_client
        .create_org_hook(organisation, url, secret, ORG_WEBHOOK_EVENTS)
        .map_err(|e| format!("cannot create Forgejo org webhook: {e}"))?;
    Ok(created.id)
}

fn is_managed_loopback_hook(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let Some((port_str, path)) = rest.split_once('/') else {
        return false;
    };
    !port_str.is_empty()
        && port_str.bytes().all(|b| b.is_ascii_digit())
        && port_str.parse::<u16>().is_ok()
        && format!("/{path}") == WEBHOOK_PATH
}

struct DedupeCache {
    entries: HashMap<String, Instant>,
    order: VecDeque<(String, Instant)>,
}

impl DedupeCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn seen_or_insert(&mut self, key: String, now: Instant) -> bool {
        while let Some(ts) = self.order.front().map(|(_, t)| *t) {
            if now.duration_since(ts) >= DEDUPE_TTL || self.entries.len() > MAX_DEDUPE_ENTRIES {
                let (k, popped_ts) = self.order.pop_front().expect("front");
                if self.entries.get(&k) == Some(&popped_ts) {
                    self.entries.remove(&k);
                }
            } else {
                break;
            }
        }
        if let Some(ts) = self.entries.get(&key)
            && now.duration_since(*ts) < DEDUPE_TTL
        {
            return true;
        }
        self.entries.insert(key.clone(), now);
        self.order.push_back((key, now));
        false
    }
}

struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_loop(
    listener: TcpListener,
    secret: Arc<String>,
    stop: Arc<AtomicBool>,
    tx: Sender<WakeEvent>,
) {
    let dedupe = Arc::new(Mutex::new(DedupeCache::new()));
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                let prev = active.fetch_add(1, Ordering::AcqRel);
                if prev >= MAX_CONCURRENT_CONNS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                    write_http(&mut stream, "503 Service Unavailable", "busy");
                    continue;
                }
                let slot = ConnSlot(Arc::clone(&active));
                let secret_c = Arc::clone(&secret);
                let stop_c = Arc::clone(&stop);
                let tx_c = tx.clone();
                let dedupe_c = Arc::clone(&dedupe);
                let spawn_res = std::thread::Builder::new()
                    .name("gewu-webhook-conn".to_owned())
                    .spawn(move || {
                        let _slot = slot;
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            handle_connection(stream, &secret_c, &stop_c, &tx_c, &dedupe_c);
                        }));
                    });
                if spawn_res.is_err() {
                    // Thread spawn failed; ConnSlot drop decrements `active` automatically.
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => {
                // Transient accept errors must never kill the webhook listener permanently.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    secret: &str,
    stop: &AtomicBool,
    tx: &Sender<WakeEvent>,
    dedupe: &Mutex<DedupeCache>,
) {
    let deadline = Instant::now() + MAX_CONN_DURATION;
    if stream.set_nonblocking(false).is_err()
        || stream.set_write_timeout(Some(Duration::from_secs(2))).is_err()
    {
        return;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let mut header_end: Option<usize> = None;
    let mut total_expected: Option<usize> = None;

    while raw.len() < MAX_WEBHOOK_BYTES {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            write_http(&mut stream, "408 Request Timeout", "timeout");
            return;
        };
        if remaining.is_zero() {
            write_http(&mut stream, "408 Request Timeout", "timeout");
            return;
        }
        if stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(500))))
            .is_err()
        {
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = MAX_WEBHOOK_BYTES.saturating_sub(raw.len());
                if n > room {
                    write_http(&mut stream, "413 Payload Too Large", "too large");
                    return;
                }
                raw.extend_from_slice(&buf[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_header_end(&raw) {
                        if pos > MAX_HEADER_BYTES {
                            write_http(&mut stream, "431 Request Header Fields Too Large", "headers too large");
                            return;
                        }
                        header_end = Some(pos);
                        let headers_str = String::from_utf8_lossy(&raw[..pos]);
                        let content_length = match parse_content_length(&headers_str) {
                            Ok(len) => len,
                            Err(()) => {
                                write_http(&mut stream, "400 Bad Request", "invalid content-length");
                                return;
                            }
                        };
                        let Some(total) = pos.checked_add(4).and_then(|s| s.checked_add(content_length)) else {
                            write_http(&mut stream, "413 Payload Too Large", "too large");
                            return;
                        };
                        if total > MAX_WEBHOOK_BYTES {
                            write_http(&mut stream, "413 Payload Too Large", "too large");
                            return;
                        }
                        total_expected = Some(total);
                    } else if raw.len() > MAX_HEADER_BYTES {
                        write_http(&mut stream, "431 Request Header Fields Too Large", "headers too large");
                        return;
                    }
                }
                if let Some(total) = total_expected
                    && raw.len() >= total
                {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    write_http(&mut stream, "408 Request Timeout", "timeout");
                    return;
                }
                continue;
            }
            Err(_) => break,
        }
    }

    let Some(pos) = header_end else {
        write_http(&mut stream, "400 Bad Request", "malformed http");
        return;
    };
    let head = String::from_utf8_lossy(&raw[..pos]);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    if method != "POST" || path != WEBHOOK_PATH {
        write_http(&mut stream, "404 Not Found", "not found");
        return;
    }

    let mut event_header = String::new();
    let mut delivery_id = String::new();
    let mut signature: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "x-forgejo-event" | "x-gitea-event" | "x-github-event" => {
                    if event_header.is_empty() {
                        event_header = val.to_owned();
                    }
                }
                "x-forgejo-delivery" | "x-gitea-delivery" | "x-github-delivery" => {
                    if delivery_id.is_empty() {
                        delivery_id = val.to_owned();
                    }
                }
                "x-forgejo-signature" | "x-gitea-signature" => {
                    signature = Some(val.to_owned());
                }
                "x-hub-signature-256" => {
                    if signature.is_none() {
                        signature = Some(val.strip_prefix("sha256=").unwrap_or(val).to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    let Some(body_start) = pos.checked_add(4) else {
        write_http(&mut stream, "400 Bad Request", "malformed http");
        return;
    };
    let body_end = total_expected.unwrap_or(raw.len()).min(raw.len());
    if body_start > body_end {
        write_http(&mut stream, "400 Bad Request", "malformed http");
        return;
    }
    let body = &raw[body_start..body_end];

    let Some(sig) = signature.as_deref() else {
        write_http(&mut stream, "401 Unauthorized", "missing signature");
        return;
    };
    if !verify_hmac_sha256_hex(secret.as_bytes(), body, sig) {
        write_http(&mut stream, "401 Unauthorized", "invalid signature");
        return;
    }

    let now = Instant::now();
    let body_digest = Sha256::digest(body);
    let mut digest_hex = String::with_capacity(64);
    for b in body_digest {
        digest_hex.push_str(&format!("{b:02x}"));
    }
    let dedupe_key = if !delivery_id.is_empty() {
        format!("id:{delivery_id}")
    } else {
        format!("body:{}:{digest_hex}", event_header.to_ascii_lowercase())
    };
    let is_duplicate = {
        let mut cache = dedupe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.seen_or_insert(dedupe_key, now)
    };
    if is_duplicate {
        write_http(&mut stream, "202 Accepted", "duplicate");
        return;
    }

    if let Ok(payload) = serde_json::from_slice::<Value>(body)
        && event_matches_signed_payload(&event_header, &payload)
        && let Some(event) = parse_wake_event(&event_header, &payload)
    {
        let _ = tx.send(event);
    }
    write_http(&mut stream, "202 Accepted", "ok");
}

/// Cross-check the unsigned `X-Forgejo-Event` header against the HMAC-signed JSON
/// payload structure so a captured signed delivery cannot be arbitrarily relabeled
/// (e.g., replaying an issue comment body as a `push` event).
fn event_matches_signed_payload(event_header: &str, payload: &Value) -> bool {
    let ev = event_header.trim().to_ascii_lowercase();
    match ev.as_str() {
        "ping" => true,
        "issues" | "issue_assign" | "issue_label" => payload.get("issue").is_some_and(Value::is_object),
        "issue_comment" => {
            payload.get("issue").is_some_and(Value::is_object)
                && payload.get("comment").is_some_and(Value::is_object)
        }
        "pull_request" | "pull_request_assign" | "pull_request_sync" => {
            payload.get("pull_request").is_some_and(Value::is_object)
        }
        "pull_request_comment" => {
            (payload.get("pull_request").is_some_and(Value::is_object)
                || payload.get("issue").is_some_and(Value::is_object))
                && payload.get("comment").is_some_and(Value::is_object)
        }
        "pull_request_approved" | "pull_request_rejected" | "pull_request_review" => {
            payload.get("pull_request").is_some_and(Value::is_object)
                && (payload.get("review").is_some_and(Value::is_object)
                    || payload.get("action").is_some())
        }
        "push" => {
            payload.get("ref").is_some_and(Value::is_string)
                && (payload.get("commits").is_some_and(Value::is_array)
                    || payload.get("after").is_some_and(Value::is_string))
        }
        "repository" => payload.get("repository").is_some_and(Value::is_object),
        _ => false,
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> Result<usize, ()> {
    let mut found: Option<usize> = None;
    for line in headers.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            if found.is_some() {
                return Err(());
            }
            found = Some(v.trim().parse::<usize>().map_err(|_| ())?);
        }
    }
    Ok(found.unwrap_or(0))
}

fn write_http(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Extract structured wake routing facts from one Forgejo webhook payload.
pub fn parse_wake_event(event_type: &str, payload: &Value) -> Option<WakeEvent> {
    let event = event_type.trim().to_ascii_lowercase();
    if event.is_empty() || event == "ping" {
        return None;
    }
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let actor = payload
        .get("sender")
        .and_then(|s| s.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let repository = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut mentioned = BTreeSet::new();
    let mut is_pr = event.starts_with("pull_request");
    let mut is_unassigned_issue = false;
    let mut number = payload.get("number").and_then(Value::as_u64);

    if let Some(issue) = payload.get("issue") {
        if number.is_none() {
            number = issue.get("number").and_then(Value::as_u64);
        }
        if issue.get("pull_request").is_some_and(|v| !v.is_null()) {
            is_pr = true;
        }
        let is_comment = matches!(event.as_str(), "issue_comment" | "pull_request_comment");
        if !is_comment
            && let Some(u) = issue.get("user").and_then(|u| u.get("login")).and_then(Value::as_str)
        {
            mentioned.insert(u.to_owned());
        }
        let mut has_assignee = false;
        if let Some(arr) = issue.get("assignees").and_then(Value::as_array) {
            for a in arr {
                if let Some(l) = a.get("login").and_then(Value::as_str) {
                    if !is_comment {
                        mentioned.insert(l.to_owned());
                    }
                    has_assignee = true;
                }
            }
        }
        if let Some(a) = issue.get("assignee").and_then(|a| a.get("login")).and_then(Value::as_str) {
            if !is_comment {
                mentioned.insert(a.to_owned());
            }
            has_assignee = true;
        }
        if !is_pr && !has_assignee {
            is_unassigned_issue = true;
        }
        if !is_comment
            && let Some(body) = issue.get("body").and_then(Value::as_str)
        {
            collect_at_mentions(body, &mut mentioned);
        }
    }

    if let Some(pr) = payload.get("pull_request") {
        is_pr = true;
        if number.is_none() {
            number = pr.get("number").and_then(Value::as_u64);
        }
        let is_comment = matches!(event.as_str(), "issue_comment" | "pull_request_comment");
        if !is_comment
            && let Some(u) = pr.get("user").and_then(|u| u.get("login")).and_then(Value::as_str)
        {
            mentioned.insert(u.to_owned());
        }
        if !is_comment
            && let Some(arr) = pr.get("assignees").and_then(Value::as_array)
        {
            for a in arr {
                if let Some(l) = a.get("login").and_then(Value::as_str) {
                    mentioned.insert(l.to_owned());
                }
            }
        }
        if !is_comment
            && let Some(body) = pr.get("body").and_then(Value::as_str)
        {
            collect_at_mentions(body, &mut mentioned);
        }
    }

    if let Some(comment) = payload.get("comment")
        && let Some(body) = comment.get("body").and_then(Value::as_str)
    {
        collect_at_mentions(body, &mut mentioned);
    }

    let summary = match (repository.is_empty(), number, action.is_empty()) {
        (false, Some(n), false) => format!("event: {event}.{action} on {repository}#{n} by {actor}"),
        (false, Some(n), true) => format!("event: {event} on {repository}#{n} by {actor}"),
        (false, None, false) => format!("event: {event}.{action} on {repository} by {actor}"),
        (false, None, true) => format!("event: {event} on {repository} by {actor}"),
        _ => format!("event: {event} by {actor}"),
    };

    Some(WakeEvent {
        event,
        action,
        actor,
        repository,
        number,
        summary,
        mentioned_logins: mentioned.into_iter().collect(),
        is_pull_request: is_pr,
        is_unassigned_issue,
        received_at: Instant::now(),
    })
}

/// Decide whether a verified Forgejo webhook event should wake one started role.
///
/// Always enforces anti-echo (`actor != role_login`), strict repository filtering,
/// and anti-loop rules (preventing reviewer/author ping-pong on comments or
/// unbounded push fan-out). Callers MUST still enforce the `Desired::Stopped`
/// iron law and per-role wake cooldown before scheduling a run.
pub fn event_matches_role(
    event: &WakeEvent,
    role_login: &str,
    permissions: &crate::engine::settings::RolePermissions,
    context: &crate::engine::settings::RoleContextConfig,
) -> bool {
    let role_login = role_login.trim();
    if role_login.is_empty() {
        return false;
    }
    // Fail closed when sender.login is missing so anonymous/unattributed payloads
    // cannot bypass anti-echo protection.
    if event.actor.trim().is_empty() || event.actor.trim().eq_ignore_ascii_case(role_login) {
        return false;
    }
    // Strict repository filter when configured on the role.
    if !context.repos.is_empty() {
        if event.repository.trim().is_empty() {
            return false;
        }
        let repo_matched = context.repos.iter().any(|r| {
            if r.contains('/') {
                r.eq_ignore_ascii_case(&event.repository)
            } else {
                // Bare repository names match only the bundled `team` org, never arbitrary owners.
                event.repository.eq_ignore_ascii_case(&format!(
                    "{}/{r}",
                    crate::bundled_forgejo::ORGANISATION
                ))
            }
        });
        if !repo_matched {
            return false;
        }
    }
    let is_comment = matches!(
        event.event.as_str(),
        "issue_comment" | "pull_request_comment"
    );
    // Directly assigned or @mentioned (for comments, require explicit @mention or
    // issue assignment — do not permanently ping-pong on every comment).
    if event
        .mentioned_logins
        .iter()
        .any(|l| l.eq_ignore_ascii_case(role_login))
    {
        return true;
    }
    // Reviewers wake on actionable PR lifecycle events (opened/reopened/synchronized/
    // ready_for_review/review_requested), NOT on every comment (prevents ping-pong).
    if event.is_pull_request && !is_comment {
        let action = event.action.trim().to_ascii_lowercase();
        let review_actionable = matches!(
            event.event.as_str(),
            "pull_request" | "pull_request_sync" | "pull_request_assign"
        ) && matches!(
            action.as_str(),
            "opened"
                | "reopened"
                | "synchronized"
                | "synchronize"
                | "ready_for_review"
                | "review_requested"
                | ""
        );
        let merge_actionable = matches!(
            event.event.as_str(),
            "pull_request_approved" | "pull_request_review" | "pull_request_sync"
        ) || (event.event == "pull_request"
            && matches!(action.as_str(), "opened" | "synchronized" | "synchronize"));
        if (permissions.review || context.views.iter().any(|v| v == "open_pulls"))
            && review_actionable
        {
            return true;
        }
        if permissions.maintain && merge_actionable {
            return true;
        }
    }
    // Triage roles / roles opting into `unassigned_issues` wake on newly opened/reopened
    // unassigned issues (not every comment).
    if event.is_unassigned_issue
        && !is_comment
        && (permissions.triage || context.views.iter().any(|v| v == "unassigned_issues"))
    {
        return true;
    }
    // Push or repository events only wake roles that explicitly watch that repository
    // via `context.repos` (prevents unbounded broadcast across every role).
    matches!(event.event.as_str(), "push" | "repository") && !context.repos.is_empty()
}

fn collect_at_mentions(text: &str, out: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if prev_ok {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_'))
                {
                    end += 1;
                }
                if end > start {
                    out.insert(text[start..end].to_owned());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
}

/// RFC 2104 HMAC-SHA256 implemented directly over `sha2::Sha256` (zero extra deps).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let out = outer.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let mac = hmac_sha256(key, data);
    let mut hex = String::with_capacity(64);
    for b in mac {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

pub fn verify_hmac_sha256_hex(key: &[u8], data: &[u8], provided_hex: &str) -> bool {
    let expected = hmac_sha256_hex(key, data);
    let provided = provided_hex.trim().to_ascii_lowercase();
    if expected.len() != provided.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(provided.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn random_hex_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("cannot read /dev/urandom for webhook secret: {e}"))?;
    f.read_exact(&mut bytes)
        .map_err(|e| format!("cannot fill webhook secret bytes: {e}"))?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::settings::{RoleContextConfig, RolePermissions};
    use serde_json::json;

    #[test]
    fn rfc4231_hmac_sha256_test_vector_matches() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        let mac = hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(verify_hmac_sha256_hex(
            b"Jefe",
            b"what do ya want for nothing?",
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        ));
        assert!(!verify_hmac_sha256_hex(
            b"Jefe",
            b"what do ya want for nothing?",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn webhook_receiver_accepts_signed_event_and_rejects_bad_signature() {
        let rx = WebhookReceiver::start().expect("webhook receiver");
        let body = serde_json::to_string(&json!({
            "action": "opened",
            "sender": { "login": "owner" },
            "repository": { "full_name": "team/workspace" },
            "issue": {
                "number": 7,
                "title": "Fix login bug",
                "body": "cc @gewu-coder",
                "assignees": [{ "login": "gewu-coder" }]
            }
        }))
        .unwrap();
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();

        // 1. Bad signature is rejected with 401.
        let bad = http
            .post(&rx.url())
            .header("X-Forgejo-Event", "issues")
            .header("X-Forgejo-Signature", "deadbeef")
            .send(body.as_str())
            .expect("http");
        assert_eq!(bad.status().as_u16(), 401);
        assert!(rx.drain_events().is_empty());

        // 2. Valid HMAC-SHA256 signature is accepted and parsed.
        let sig = hmac_sha256_hex(rx.secret().as_bytes(), body.as_bytes());
        let ok = http
            .post(&rx.url())
            .header("X-Forgejo-Event", "issues")
            .header("X-Forgejo-Signature", &sig)
            .send(body.as_str())
            .expect("http");
        assert_eq!(ok.status().as_u16(), 202);
        let events = rx.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].repository, "team/workspace");
        assert_eq!(events[0].number, Some(7));
        assert!(events[0].mentioned_logins.contains(&"gewu-coder".to_owned()));
    }

    #[test]
    fn anti_echo_and_role_routing_work_as_designed() {
        let pr_event = parse_wake_event(
            "pull_request",
            &json!({
                "action": "opened",
                "sender": { "login": "gewu-coder" },
                "repository": { "full_name": "team/workspace" },
                "pull_request": { "number": 3, "user": { "login": "gewu-coder" } }
            }),
        )
        .unwrap();
        let default_ctx = RoleContextConfig::default();
        let default_perm = RolePermissions::default();
        let review_perm = RolePermissions {
            review: true,
            ..RolePermissions::default()
        };
        // Anti-echo: coder must NOT wake on its own PR opening.
        assert!(!event_matches_role(&pr_event, "gewu-coder", &default_perm, &default_ctx));
        // Reviewer MUST wake on coder's PR opening.
        assert!(event_matches_role(&pr_event, "gewu-reviewer", &review_perm, &default_ctx));
        // Unrelated contributor with default permissions does NOT wake on another's PR.
        assert!(!event_matches_role(&pr_event, "gewu-other", &default_perm, &default_ctx));

        // Empty sender must fail closed (never bypass anti-echo).
        let anon = WakeEvent {
            actor: String::new(),
            ..pr_event.clone()
        };
        assert!(!event_matches_role(&anon, "gewu-reviewer", &review_perm, &default_ctx));

        // Comment ping-pong: PR comment without explicit @mention must NOT wake reviewer.
        let pr_comment = parse_wake_event(
            "pull_request_comment",
            &json!({
                "action": "created",
                "sender": { "login": "gewu-coder" },
                "repository": { "full_name": "team/workspace" },
                "issue": { "number": 3, "pull_request": {}, "user": { "login": "gewu-coder" } },
                "comment": { "body": "thanks, updated!" }
            }),
        )
        .unwrap();
        assert!(!event_matches_role(&pr_comment, "gewu-reviewer", &review_perm, &default_ctx));
    }

    #[test]
    fn webhook_content_length_overflow_returns_413_and_keeps_listener_alive() {
        let rx = WebhookReceiver::start().expect("webhook receiver");
        let addr = rx
            .url()
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        // 1. Send adversarial Content-Length: usize::MAX — must NOT panic or kill thread.
        let mut raw = TcpStream::connect(&addr).expect("connect raw");
        let bad_req = format!(
            "POST {WEBHOOK_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        raw.write_all(bad_req.as_bytes()).expect("write");
        let mut resp = String::new();
        let _ = raw.read_to_string(&mut resp);
        assert!(resp.starts_with("HTTP/1.1 413"), "expected 413, got: {resp}");
        assert!(rx.drain_events().is_empty());

        // 2. Subsequent valid HMAC-signed event MUST still be accepted (listener alive).
        let body = serde_json::to_string(&json!({
            "action": "opened",
            "sender": { "login": "owner" },
            "repository": { "full_name": "team/workspace" },
            "issue": { "number": 42, "title": "Alive", "assignees": [{ "login": "gewu-coder" }] }
        }))
        .unwrap();
        let sig = hmac_sha256_hex(rx.secret().as_bytes(), body.as_bytes());
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let ok = http
            .post(&rx.url())
            .header("X-Forgejo-Event", "issues")
            .header("X-Forgejo-Delivery", "delivery-unique-1")
            .header("X-Forgejo-Signature", &sig)
            .send(body.as_str())
            .expect("listener still alive");
        assert_eq!(ok.status().as_u16(), 202);
        // 3. Replaying the same delivery ID is deduplicated.
        let dup = http
            .post(&rx.url())
            .header("X-Forgejo-Event", "issues")
            .header("X-Forgejo-Delivery", "delivery-unique-1")
            .header("X-Forgejo-Signature", &sig)
            .send(body.as_str())
            .expect("dup");
        assert_eq!(dup.status().as_u16(), 202);
        // 4. Relabeling the signed issue body as `push` is dropped by payload cross-check.
        let relabeled = http
            .post(&rx.url())
            .header("X-Forgejo-Event", "push")
            .header("X-Forgejo-Delivery", "delivery-relabel-2")
            .header("X-Forgejo-Signature", &sig)
            .send(body.as_str())
            .expect("relabel");
        assert_eq!(relabeled.status().as_u16(), 202);
        let events = rx.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].number, Some(42));
    }
}
