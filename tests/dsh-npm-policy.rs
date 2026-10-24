//! A real npm run of the reviewed `allowScripts` mechanism, with a loopback
//! registry and a denied lifecycle script that writes a marker. This is the
//! semantic proof the scripted-npm tests cannot give: on a supported npm the
//! marker must be absent, and the dangerous global switch must make it appear.
//!
//! The test skips when the machine's npm predates 11.16 (the installer refuses
//! that npm for DSH); it never uses a real provider, key or product home.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha512};
use support::Sandbox;

struct Registry {
    url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Registry {
    fn start(tarball: Vec<u8>, integrity: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback registry");
        listener
            .set_nonblocking(true)
            .expect("a nonblocking registry");
        let address = listener.local_addr().expect("the registry address");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let metadata = format!(
                "{{\"name\":\"fixture-dep\",\"dist-tags\":{{\"latest\":\"1.0.0\"}},\"versions\":{{\"1.0.0\":{{\"name\":\"fixture-dep\",\"version\":\"1.0.0\",\"license\":\"MIT\",\"scripts\":{{\"postinstall\":\"node postinstall.js\"}},\"dist\":{{\"tarball\":\"http://{address}/fixture-dep/-/fixture-dep-1.0.0.tgz\",\"integrity\":\"{integrity}\"}}}}}}}}"
            );
            loop {
                if thread_stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 2048];
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    request.extend_from_slice(&buffer[..read]);
                                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                            }
                        }
                        let path = String::from_utf8_lossy(&request)
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("")
                            .to_owned();
                        let body = if path.ends_with(".tgz") {
                            Some(tarball.clone())
                        } else if path.starts_with("/fixture-dep") {
                            Some(metadata.as_bytes().to_vec())
                        } else {
                            None
                        };
                        let (status, content_type, body) = match body {
                            Some(body) => ("200 OK", "application/octet-stream", body),
                            None => ("404 Not Found", "text/plain", Vec::new()),
                        };
                        let header = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        if stream.write_all(header.as_bytes()).is_err()
                            || stream.write_all(&body).is_err()
                        {
                            continue;
                        }
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}/"),
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture directory");
    }
    fs::write(path, text).expect("the fixture file");
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let joined = (first << 16) | (second << 8) | third;
        text.push(TABLE[((joined >> 18) & 63) as usize] as char);
        text.push(TABLE[((joined >> 12) & 63) as usize] as char);
        text.push(if chunk.len() > 1 {
            TABLE[((joined >> 6) & 63) as usize] as char
        } else {
            '='
        });
        text.push(if chunk.len() > 2 {
            TABLE[(joined & 63) as usize] as char
        } else {
            '='
        });
    }
    text
}

fn npm_supports_allow_scripts() -> bool {
    let output = match Command::new("npm").arg("--version").output() {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let core = version.split('-').next().unwrap_or(&version);
    let mut pieces = core.split('.');
    let major: u64 = pieces
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    let minor: u64 = pieces
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    (major, minor) >= (11, 16)
}

fn run_case(
    sandbox: &Sandbox,
    template: &Path,
    registry: &str,
    marker: &Path,
    dangerous: bool,
) -> std::process::Output {
    let root = sandbox.root.join(format!(
        "run-{}-{}",
        dangerous,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&root).expect("the run root");
    fs::copy(template.join("package.json"), root.join("package.json")).expect("the package file");
    let lock = fs::read_to_string(template.join("package-lock.json")).expect("the lock");
    fs::write(root.join("package-lock.json"), lock).expect("the lock copy");
    let home = sandbox.root.join(format!("home-{dangerous}"));
    let cache = sandbox.root.join(format!("cache-{dangerous}"));
    fs::create_dir_all(&home).expect("the npm home");
    fs::create_dir_all(&cache).expect("the npm cache");
    let mut command = Command::new("npm");
    command
        .args(["ci", "--no-audit", "--no-fund", "--no-progress", "--prefix"])
        .arg(&root)
        .env("HOME", &home)
        .env("npm_config_cache", &cache)
        .env("npm_config_registry", registry)
        .env("npm_config_update_notifier", "false")
        .env("npm_config_audit", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_progress", "false")
        .env("FIXTURE_MARKER", marker)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .env_remove("npm_config_dangerously_allow_all_scripts");
    if dangerous {
        command.env("npm_config_dangerously_allow_all_scripts", "true");
    }
    command.stdin(Stdio::null()).output().expect("npm ci runs")
}

#[test]
fn the_real_supported_npm_skips_the_denied_lifecycle_script() {
    if !npm_supports_allow_scripts() {
        eprintln!(
            "skipping: this machine's npm predates 11.16, which the installer refuses for DSH"
        );
        return;
    }
    let sandbox = Sandbox::new("dsh-real-npm-policy");
    let package = sandbox.root.join("package/package");
    write(
        &package.join("package.json"),
        r#"{"name":"fixture-dep","version":"1.0.0","license":"MIT","scripts":{"postinstall":"node postinstall.js"}}"#,
    );
    write(
        &package.join("postinstall.js"),
        "const fs=require('fs'); fs.writeFileSync(process.env.FIXTURE_MARKER,'postinstall ran\\n');\n",
    );
    let tarball = sandbox.root.join("fixture-dep-1.0.0.tgz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(sandbox.root.join("package"))
        .arg("package")
        .status()
        .expect("tar runs");
    assert!(status.success());
    let bytes = fs::read(&tarball).expect("the tarball");
    let integrity = format!("sha512-{}", base64(&Sha512::digest(&bytes)));
    let registry = Registry::start(bytes, integrity.clone());
    let template = sandbox.root.join("template");
    write(
        &template.join("package.json"),
        r#"{"name":"fixture-root","version":"1.0.0","private":true,"dependencies":{"fixture-dep":"1.0.0"},"allowScripts":{"fixture-dep@1.0.0":false}}"#,
    );
    let lock = format!(
        "{{\"name\":\"fixture-root\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"requires\":true,\"packages\":{{\"\":{{\"name\":\"fixture-root\",\"version\":\"1.0.0\",\"dependencies\":{{\"fixture-dep\":\"1.0.0\"}}}},\"node_modules/fixture-dep\":{{\"version\":\"1.0.0\",\"resolved\":\"{}fixture-dep/-/fixture-dep-1.0.0.tgz\",\"integrity\":\"{integrity}\",\"hasInstallScript\":true,\"license\":\"MIT\"}}}}}}",
        registry.url
    );
    write(&template.join("package-lock.json"), &lock);

    let normal = sandbox.root.join("marker-normal");
    let output = run_case(&sandbox, &template, &registry.url, &normal, false);
    assert!(
        output.status.success(),
        "npm ci failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !normal.exists(),
        "a denied postinstall ran on supported npm"
    );

    let bypass = sandbox.root.join("marker-bypass");
    let output = run_case(&sandbox, &template, &registry.url, &bypass, true);
    assert!(
        output.status.success(),
        "the bypass control failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        bypass.exists(),
        "the fixture itself did not execute under dangerously-allow-all-scripts"
    );
}
