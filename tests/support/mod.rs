#![allow(dead_code)]
//! Shared harness for the installer integration tests. Every server, HOME and data
//! root is task-private; no real product home or provider store is used.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub tmp: PathBuf,
    pub runtime: PathBuf,
    pub bin: PathBuf,
}

impl Sandbox {
    pub fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-installer-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("a sandbox root");
        let mut paths = Vec::new();
        for name in [
            "home", "data", "config", "state", "cache", "tmp", "runtime", "bin",
        ] {
            let path = root.join(name);
            fs::create_dir_all(&path).expect("a sandbox directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("the sandbox mode");
            paths.push(path);
        }
        let [home, data, config, state, cache, tmp, runtime, bin] = paths
            .try_into()
            .unwrap_or_else(|_| panic!("eight sandbox paths"));
        Self {
            root,
            home,
            data,
            config,
            state,
            cache,
            tmp,
            runtime,
            bin,
        }
    }

    pub fn product(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("TMPDIR", &self.tmp)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .env(
                "PATH",
                format!("{}:/usr/local/bin:/usr/bin:/bin", self.bin.display()),
            );
        command
    }

    pub fn data_root(&self) -> PathBuf {
        self.data.join("gewu-multiverse")
    }

    pub fn harness_dir(&self, program: &str) -> PathBuf {
        self.data_root().join("harnesses").join(program)
    }

    pub fn install_root(&self, program: &str, version: &str) -> PathBuf {
        self.harness_dir(program).join(version)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn write_executable(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the executable directory");
    }
    fs::write(path, text).expect("the executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("the executable mode");
}

pub fn write_private(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the file directory");
    }
    fs::write(path, bytes).expect("the file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("the private mode");
}

/// A real Pi-shaped tarball: `pi/pi` is an executable script.
pub fn pi_archive(directory: &Path) -> Vec<u8> {
    let payload = directory.join("pi-payload");
    let _ = fs::remove_dir_all(&payload);
    write_executable(&payload.join("pi/pi"), "#!/bin/sh\necho pi-fixture\n");
    fs::write(payload.join("pi/README"), "fixture\n").expect("the fixture readme");
    let archive = directory.join("pi.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&payload)
        .arg("pi")
        .status()
        .expect("tar runs");
    assert!(status.success(), "the fixture archive is made");
    fs::read(&archive).expect("the fixture archive")
}

/// An install record and executable as an earlier product version could have left
/// them. `record_version` may differ from the directory's pinned version to make the
/// installer treat the root as an older/carried version and reinstall over it.
pub fn pi_old_install(root: &Path, record_version: &str) {
    let executable = root.join("pi/pi");
    write_executable(&executable, "#!/bin/sh\necho old-pi\n");
    let record = serde_json::json!({
        "program": "pi",
        "version": record_version,
        "sha256": "fixture",
        "license": "MIT",
        "entrypoint": "pi/pi",
        "resources": null,
        "installed_at": "2026-10-23T00:00:00.000Z",
    });
    write_private(
        &root.join("install.json"),
        serde_json::to_string_pretty(&record)
            .expect("the record")
            .as_bytes(),
    );
}

/// A DeepSeek Harness-shaped install with the required native helpers.
pub fn dsh_old_install(root: &Path, record_version: &str, spawn_helper_mode: u32) {
    let executable = root.join("node_modules/.bin/dsh");
    write_executable(&executable, "#!/bin/sh\necho 0.2.1-rc.2\n");
    let platform = node_platform();
    write_private(
        &root.join("node_modules/@deepseek-ai/dsh/package.json"),
        br#"{"name":"@deepseek-ai/dsh","version":"0.2.1-rc.2"}"#,
    );
    write_private(
        &root.join("node_modules/koffi/package.json"),
        br#"{"name":"koffi","version":"3.1.1"}"#,
    );
    write_private(
        &root.join("node_modules/@koromix/koffi-linux-x64/linux_x64/koffi.node"),
        b"fixture-native",
    );
    write_private(
        &root.join(format!(
            "node_modules/node-pty/prebuilds/{platform}/pty.node"
        )),
        b"fixture-pty",
    );
    if spawn_helper_mode != 0 {
        let helper = root.join(format!(
            "node_modules/node-pty/prebuilds/{platform}/spawn-helper"
        ));
        write_private(&helper, b"fixture-spawn-helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(spawn_helper_mode))
            .expect("the spawn-helper mode");
    }
    let record = serde_json::json!({
        "program": "dsh",
        "version": record_version,
        "sha256": "40e3d025c0240c4316ff42dee9bedc7709c0fb38aea10543b47076ed2af52e1d",
        "license": "MIT",
        "entrypoint": "node_modules/.bin/dsh",
        "resources": null,
        "installed_at": "2026-10-23T00:00:00.000Z",
    });
    write_private(
        &root.join("install.json"),
        serde_json::to_string_pretty(&record)
            .expect("the record")
            .as_bytes(),
    );
}

pub fn node_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

/// One request-only HTTP server. The body is sent in chunks with an optional pause,
/// so a download can be slowed and cancelled deterministically.
pub struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    pub fn body(body: Vec<u8>, chunk: usize, pause: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener
            .set_nonblocking(true)
            .expect("a nonblocking listener");
        let addr = listener.local_addr().expect("the loopback address");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if thread_stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        serve(&mut stream, &body, chunk.max(1), pause, &thread_stop);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{addr}/package"),
            stop,
            handle: Some(handle),
        }
    }

    pub fn set_on(&self, command: &mut Command, sha256: &str) {
        command
            .env("GEWU_INSTALL_TEST_URL", &self.url)
            .env("GEWU_INSTALL_TEST_SHA256", sha256);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(stream: &mut TcpStream, body: &[u8], chunk: usize, pause: Duration, stop: &AtomicBool) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    for part in body.chunks(chunk) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if stream.write_all(part).is_err() || stream.flush().is_err() {
            return;
        }
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
}

pub fn wait_until(mut test: impl FnMut() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if test() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    test()
}

pub fn stage_dirs(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".stage-"))
        .map(|entry| entry.path())
        .collect()
}

pub fn signal(child: &std::process::Child, signal: &str) {
    let status = Command::new("/bin/kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "the signal is delivered");
}
