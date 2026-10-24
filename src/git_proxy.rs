//! A run-private, fixed-target Git smart-HTTP credential transport.
//!
//! The native program never receives the Forgejo token or an askpass path. It
//! sends ordinary Git HTTP requests through this loopback proxy; the supervisor
//! adds the agent's existing Basic credential and forwards only the configured
//! Forgejo origin's smart-HTTP paths. It does not follow redirects, proxy APIs or
//! accept a different target.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct GitProxy {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl GitProxy {
    /// Start a proxy for one run, with the token held only by the supervisor.
    pub fn start(run_dir: &Path, agent: &str, credentials: &Path) -> Result<Option<Self>, String> {
        let Some(context) = crate::run::read_forgejo_context(run_dir) else {
            return Ok(None);
        };
        let Ok(token) = crate::credentials::forgejo_agent_token(credentials, agent) else {
            return Ok(None);
        };
        let Some((host, port, scheme)) = parse_origin(&context.url) else {
            return Err("the Forgejo address is not a usable HTTP origin".to_owned());
        };
        if scheme != "http" {
            return Err(
                "Git credential transport currently requires an http Forgejo origin".to_owned(),
            );
        }
        let repos: Vec<String> = context
            .repositories
            .iter()
            .map(|repo| repo.full_name.to_ascii_lowercase())
            .collect();
        if repos.is_empty() {
            return Ok(None);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("the run-private Git transport cannot listen: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("the run-private Git transport cannot poll: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("the run-private Git transport has no address: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let auth = basic_auth(&context.login, token.expose());
        let token_guard = token;
        let handle = std::thread::spawn(move || {
            let _token = token_guard;
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve(stream, &host, port, &repos, &auth);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Some(Self {
            address,
            stop,
            handle: Some(handle),
        }))
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for GitProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn parse_origin(url: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse().ok()?),
        None => (
            authority.to_owned(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    Some((host, port, scheme.to_ascii_lowercase()))
}

fn basic_auth(login: &str, token: &str) -> String {
    format!("Basic {}", base64(format!("{login}:{token}").as_bytes()))
}

fn base64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn read_headers(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > 128 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(buf)
}

fn serve(mut client: TcpStream, host: &str, port: u16, repos: &[String], auth: &str) -> Option<()> {
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    client
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let head = read_headers(&mut client)?;
    let split = head.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let header_text = String::from_utf8_lossy(&head[..split]).into_owned();
    let mut lines = header_text.split("\r\n");
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_ascii_uppercase();
    let target = request.next()?;
    if method != "GET" && method != "POST" {
        return None;
    }
    let path = target.strip_prefix("http://")?;
    let (authority, path) = path.split_once('/')?;
    let path = format!("/{path}");
    if authority != format!("{host}:{port}") {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    let repo_ok = repos
        .iter()
        .any(|repo| lower.starts_with(&format!("/{repo}.git/")));
    if !repo_ok
        || !(lower.contains("/info/refs")
            || lower.ends_with("/git-upload-pack")
            || lower.ends_with("/git-receive-pack"))
    {
        return None;
    }
    let content_length = lines
        .clone()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = head[split..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 8192];
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let body = &body[..content_length.min(body.len())];
    let mut upstream = TcpStream::connect_timeout(
        &format!("{host}:{port}").parse().ok()?,
        Duration::from_secs(10),
    )
    .ok()?;
    upstream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    upstream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    let mut out =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: {auth}\r\n");
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    upstream.write_all(out.as_bytes()).ok()?;
    upstream.write_all(body).ok()?;
    let mut response = Vec::new();
    upstream.read_to_end(&mut response).ok()?;
    client.write_all(&response).ok()?;
    Some(())
}
