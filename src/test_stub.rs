//! A tiny local HTTP server, used only by this crate's tests and demonstrations.
//!
//! It listens on `127.0.0.1` on a port the operating system chooses, answers a
//! scripted list of replies, and records every request it received. It exists so the
//! model client's whole error taxonomy — a normal exchange, a tool call, a malformed
//! reply, a bad key, a rate limit, a timeout — can be exercised without a network and
//! without spending anyone's money.
//!
//! **It is not part of the application.** The terminal program never starts it, never
//! imports it, and never listens on a socket. It is `#[doc(hidden)]` for that reason:
//! it is here so the tests are honest, not so the tool has a server.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One scripted reply the stub will give.
#[derive(Debug, Clone)]
pub struct StubReply {
    /// The HTTP status to answer with.
    pub status: u16,
    /// The body to answer with.
    pub body: String,
    /// A `Retry-After` to send, when one should be sent.
    pub retry_after_secs: Option<u64>,
    /// How long to wait before answering, so a timeout can be exercised.
    pub delay: Duration,
}

impl StubReply {
    /// A `200` with a JSON body.
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            retry_after_secs: None,
            delay: Duration::ZERO,
        }
    }

    /// Some other status with a body.
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            retry_after_secs: None,
            delay: Duration::ZERO,
        }
    }
}

/// One request the stub received, kept whole.
#[derive(Debug, Clone, Default)]
pub struct StubRequest {
    /// The method, as written.
    pub method: String,
    /// The path, as written.
    pub path: String,
    /// The headers, lowercased names as written by the client.
    pub headers: Vec<(String, String)>,
    /// The body, exactly.
    pub body: String,
}

impl StubRequest {
    /// A header's value, by name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

/// A running stub server. Dropping it stops the accept loop.
pub struct Stub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Stub {
    /// Start a stub that answers the given replies, in order; the last one repeats.
    pub fn start(replies: Vec<StubReply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the stub can bind a local port");
        let addr = listener.local_addr().expect("the stub knows its port");
        listener
            .set_nonblocking(true)
            .expect("the stub can poll its listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || serve(listener, replies, requests, stop))
        };
        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    /// The URL a client should post a chat request to.
    pub fn url(&self) -> String {
        format!("http://{}/v1/chat/completions", self.addr)
    }

    /// The address the stub is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<StubRequest> {
        self.requests
            .lock()
            .expect("the request list is not poisoned")
            .clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Accept connections until the stub is dropped.
fn serve(
    listener: TcpListener,
    replies: Vec<StubReply>,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    stop: Arc<AtomicBool>,
) {
    let mut next = 0usize;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // The last scripted reply repeats, so a test can leave a stub up for
                // a while without scripting every connection.
                let reply = if replies.is_empty() {
                    StubReply::status(500, "the stub has no scripted reply")
                } else {
                    replies[next.min(replies.len() - 1)].clone()
                };
                if next < replies.len() {
                    next += 1;
                }
                let requests = Arc::clone(&requests);
                // One thread per connection, so a scripted delay never blocks the
                // accept loop and dropping the stub never waits for that delay.
                thread::spawn(move || {
                    let _ = handle_connection(stream, reply, requests);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

/// Read one request, record it, and answer it.
fn handle_connection(
    mut stream: TcpStream,
    reply: StubReply,
    requests: Arc<Mutex<Vec<StubRequest>>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at + 4;
        }
        if buffer.len() > 1024 * 1024 {
            return Ok(());
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let (method, path, headers) = parse_head(&head);
    let content_length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_end = (header_end + content_length).min(buffer.len());
    let body = String::from_utf8_lossy(&buffer[header_end..body_end]).into_owned();
    requests
        .lock()
        .expect("the request list is not poisoned")
        .push(StubRequest {
            method,
            path,
            headers,
            body,
        });

    if !reply.delay.is_zero() {
        thread::sleep(reply.delay);
    }

    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status),
        reply.body.len()
    );
    if let Some(secs) = reply.retry_after_secs {
        response.push_str(&format!("Retry-After: {secs}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&reply.body);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    Ok(())
}

/// The method, path and headers of a request head.
fn parse_head(head: &str) -> (String, String, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    (method, path, headers)
}

/// The reason phrase for a status, as far as the tests need one.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Server Error",
        _ => "Unknown",
    }
}

/// Where a byte pattern first appears.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
