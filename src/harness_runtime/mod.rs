//! Per-role resident harness server/RPC pool (`HarnessRuntime`).
//!
//! Each active native role owns at most ONE dedicated, Landlock-confined resident
//! harness process (`dsh --profile sdk`, `pi --mode rpc`, `codex app-server`, or
//! `opencode serve`). Runtimes are NEVER shared across roles, keys, workspaces, or
//! permission tiers.
//!
//! Every wakeup opens a strictly **fresh ephemeral session** (`sessionId = run_id`,
//! `new_session`, `thread/start { ephemeral: true }`, or `--archived` run + delete)
//! and discards it at round completion so zero cross-round chat history leaks.
//! Idle runtimes self-park after `runtime_idle_secs` (default 300s) to reclaim RAM,
//! and any transport failure, timeout, or mid-turn abort immediately recycles the
//! underlying harness process.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::credentials::{self, Secret};
use crate::engine::settings::RolePermissions;
use crate::harness::{Program, Protocol};
use crate::mcp_forgejo::ActiveRunPointer;
use crate::role_mcp::{self, ExternalMcpServer};
use crate::run::RunForgejo;

/// File name of the role runtime socket inside `<state-root>/<agent>/runtime/`.
pub const RUNTIME_SOCKET_FILE: &str = "runtime.sock";
/// File name of the role runtime pid marker inside `<state-root>/<agent>/runtime/`.
pub const RUNTIME_PID_FILE: &str = "runtime.pid";
/// File name of the boot-id + start-time metadata alongside `runtime.pid`.
pub const RUNTIME_META_FILE: &str = "runtime.meta";
/// File name of the non-secret runtime launch spec.
pub const RUNTIME_SPEC_FILE: &str = "runtime-spec.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeMeta {
    pid: u32,
    boot_id: String,
    start_time: u64,
}

/// Specification written by the engine (mode 0600 under state_root, inaccessible
/// to Landlock-confined agents) before spawning `--role-runtime`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRuntimeSpec {
    pub agent: String,
    pub fingerprint: String,
    /// Per-boot 256-bit random capability required on every `runtime.sock` request.
    pub auth_cookie: String,
    /// Authoritative `<state-root>/<agent>` directory under which `run_dir` must live.
    pub runs_root: PathBuf,
    pub workspace: PathBuf,
    pub skills_dir: PathBuf,
    pub credentials: PathBuf,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub harness: String,
    pub version: String,
    pub program: PathBuf,
    pub install_root: PathBuf,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub codex_sandbox: String,
    pub timeout_secs: u64,
    pub idle_secs: u64,
    #[serde(default)]
    pub permissions: RolePermissions,
    #[serde(default)]
    pub role_env: BTreeMap<String, String>,
    #[serde(default)]
    pub role_mcp_config: Option<PathBuf>,
    #[serde(default)]
    pub forgejo: Option<RunForgejo>,
}

/// Request sent from `--run-agent` to its role's `RoleRuntimeHost` over `runtime.sock`.
///
/// Every variant carries the per-boot 256-bit `cookie`. Permissions are NEVER
/// accepted from clients — the host enforces its immutable `RoleRuntimeSpec.permissions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeRequest {
    Ping {
        cookie: String,
    },
    Shutdown {
        cookie: String,
    },
    Execute {
        cookie: String,
        run_id: String,
        run_dir: PathBuf,
        prompt: String,
        timeout_secs: u64,
        ledger: PathBuf,
        counter: PathBuf,
    },
}

impl RuntimeRequest {
    fn cookie(&self) -> &str {
        match self {
            Self::Ping { cookie }
            | Self::Shutdown { cookie }
            | Self::Execute { cookie, .. } => cookie,
        }
    }
}

/// Streamed response frame sent from `RoleRuntimeHost` back to `--run-agent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeFrame {
    Pong,
    Event { line: String },
    Stderr { line: String },
    Done { exit_code: i32 },
    Error { message: String },
}

/// Compute a stable configuration fingerprint for one role's resident runtime.
///
/// Any change in harness version/binary, model/provider, permissions, skills,
/// `mcp.json`, Forgejo repos, or role env/secrets yields a new fingerprint so
/// `HarnessPool` transparently recycles the host.
#[allow(clippy::too_many_arguments)]
pub fn compute_fingerprint(
    agent: &str,
    program: Program,
    version: &str,
    executable: &Path,
    protocol: Protocol,
    base_url: &str,
    model: &str,
    codex_sandbox: &str,
    permissions: &RolePermissions,
    role_env: &BTreeMap<String, String>,
    role_secrets: &BTreeMap<String, Secret>,
    skills_dir: &Path,
    mcp_config: Option<&Path>,
    forgejo: Option<&RunForgejo>,
) -> String {
    let mut hasher = Sha256::new();
    let feed = |h: &mut Sha256, label: &str, value: &str| {
        h.update(label.as_bytes());
        h.update(b":");
        h.update(value.as_bytes());
        h.update(b"\n");
    };
    feed(&mut hasher, "agent", agent);
    feed(&mut hasher, "program", program.id());
    feed(&mut hasher, "version", version);
    feed(&mut hasher, "exe", &executable.display().to_string());
    feed(&mut hasher, "protocol", protocol.id());
    feed(&mut hasher, "base_url", base_url);
    feed(&mut hasher, "model", model);
    feed(&mut hasher, "codex_sandbox", codex_sandbox);
    if let Ok(p) = serde_json::to_string(permissions) {
        feed(&mut hasher, "permissions", &p);
    }
    for (k, v) in role_env {
        feed(&mut hasher, "env_k", k);
        feed(&mut hasher, "env_v", v);
    }
    for (k, secret) in role_secrets {
        feed(&mut hasher, "sec_k", k);
        let digest = Sha256::digest(secret.expose().as_bytes());
        hasher.update(digest);
    }
    if let Some(fj) = forgejo
        && let Ok(j) = serde_json::to_string(fj)
    {
        feed(&mut hasher, "forgejo", &j);
    }
    if let Some(mcp_path) = mcp_config
        && let Ok(bytes) = fs::read(mcp_path)
    {
        hasher.update(b"mcp:");
        hasher.update(bytes);
    }
    hash_dir_tree(&mut hasher, skills_dir);
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in out {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn hash_dir_tree(hasher: &mut Sha256, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            hasher.update(name.as_bytes());
            hasher.update(b"/");
        }
        if path.is_dir() {
            hash_dir_tree(hasher, &path);
        } else if let Ok(bytes) = fs::read(&path) {
            hasher.update(&bytes);
        }
    }
}

/// Engine-owned pool of per-role resident harness hosts.
pub struct HarnessPool {
    entries: HashMap<String, PoolEntry>,
}

struct PoolEntry {
    fingerprint: String,
    auth_cookie: String,
    socket: PathBuf,
    pid_file: PathBuf,
    /// Held open for the lifetime of the pool entry; EOF on `--role-runtime`'s stdin
    /// signals cleanly if the Engine process exits (even on SIGKILL) without the
    /// thread-bound pitfalls of Linux `PR_SET_PDEATHSIG`.
    _keepalive: Option<ChildStdin>,
    child: Child,
}

impl Default for HarnessPool {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessPool {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Runtime directory for one role under `<state-root>/<agent>/runtime/`.
    pub fn role_runtime_dir(state_root: &Path, agent: &str) -> PathBuf {
        state_root.join(agent).join("runtime")
    }

    /// Ensure a live per-role resident runtime matching `spec.fingerprint` is ready,
    /// returning `(socket_path, auth_cookie)`. Spawns a fresh host in its own
    /// process group if absent, exited, or mismatched fingerprint.
    pub fn ensure_ready(
        &mut self,
        product_binary: &Path,
        spec: &RoleRuntimeSpec,
    ) -> Result<(PathBuf, String), String> {
        if let Some(entry) = self.entries.get_mut(&spec.agent) {
            let alive = entry.child.try_wait().ok().flatten().is_none();
            if alive
                && entry.fingerprint == spec.fingerprint
                && ping_socket(&entry.socket, &entry.auth_cookie)
            {
                return Ok((entry.socket.clone(), entry.auth_cookie.clone()));
            }
            self.stop_role(&spec.agent);
        }

        if let Some(parent) = spec.socket.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("role runtime directory cannot be created: {e}"))?;
            let _ = crate::mode::set_mode(parent, crate::mode::PRIVATE_DIR_MODE);
        }
        let auth_cookie = crate::mode::random_hex_256()
            .map_err(|e| format!("role runtime auth cookie cannot be generated: {e}"))?;
        let mut owned_spec = spec.clone();
        owned_spec.auth_cookie = auth_cookie.clone();
        let spec_path = spec
            .socket
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(RUNTIME_SPEC_FILE);
        let spec_json = serde_json::to_vec_pretty(&owned_spec)
            .map_err(|e| format!("role runtime spec cannot be encoded: {e}"))?;
        crate::mode::write_private_nofollow(&spec_path, &spec_json)
            .map_err(|e| format!("role runtime spec cannot be written: {e}"))?;
        let _ = fs::remove_file(&spec.socket);

        let mut cmd = Command::new(product_binary);
        cmd.arg("--role-runtime")
            .arg(&spec_path)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("role runtime host cannot start: {e}"))?;
        let keepalive = child.stdin.take();

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if spec.socket.exists() && ping_socket(&spec.socket, &auth_cookie) {
                self.entries.insert(
                    spec.agent.clone(),
                    PoolEntry {
                        fingerprint: spec.fingerprint.clone(),
                        auth_cookie: auth_cookie.clone(),
                        socket: spec.socket.clone(),
                        pid_file: spec.pid_file.clone(),
                        _keepalive: keepalive,
                        child,
                    },
                );
                return Ok((spec.socket.clone(), auth_cookie));
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }

        crate::run::adapter_signal_run_group(child.id(), true);
        let _ = child.kill();
        let _ = child.wait();
        let mut err_text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut err_text);
        }
        let _ = fs::remove_file(&spec.socket);
        Err(format!(
            "role runtime for `{}` did not become ready: {}",
            spec.agent,
            err_text.lines().next().unwrap_or("startup timeout")
        ))
    }

    /// Reap any host that has self-parked (idle timeout) or exited, sweeping its
    /// entire process group so an OOM/SIGKILLed host never leaves orphaned MCP or
    /// harness children behind.
    pub fn reap_exited(&mut self) {
        let mut dead = Vec::new();
        for (agent, entry) in &mut self.entries {
            if matches!(entry.child.try_wait(), Ok(Some(_))) {
                dead.push(agent.clone());
            }
        }
        for agent in dead {
            if let Some(entry) = self.entries.remove(&agent) {
                crate::run::adapter_signal_run_group(entry.child.id(), true);
                let _ = fs::remove_file(&entry.socket);
                let _ = fs::remove_file(&entry.pid_file);
            }
        }
    }

    /// Stop one role's resident runtime cleanly, killing its entire process group
    /// (`killpg`) on timeout so zero driver/MCP/OpenCode descendants survive.
    pub fn stop_role(&mut self, agent: &str) {
        let Some(mut entry) = self.entries.remove(agent) else {
            return;
        };
        send_shutdown_socket(&entry.socket, &entry.auth_cookie);
        drop(entry._keepalive.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(entry.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let pgid = entry.child.id();
        if matches!(entry.child.try_wait(), Ok(None)) {
            crate::run::adapter_signal_run_group(pgid, false);
            std::thread::sleep(Duration::from_millis(150));
            crate::run::adapter_signal_run_group(pgid, true);
            let _ = entry.child.kill();
            let _ = entry.child.wait();
        } else {
            // Also sweep any lingering descendants that stayed in the process group.
            crate::run::adapter_signal_run_group(pgid, true);
        }
        let _ = fs::remove_file(&entry.socket);
        let _ = fs::remove_file(&entry.pid_file);
    }

    /// Stop all resident role runtimes on engine shutdown.
    pub fn stop_all(&mut self) {
        let agents: Vec<String> = self.entries.keys().cloned().collect();
        for agent in agents {
            self.stop_role(&agent);
        }
    }

    /// Clean up stale runtime sockets/pid files, kill orphaned role-runtime process
    /// groups left by a crashed earlier engine, and remove dead `/tmp/gewu-rt-*` trees.
    pub fn recover_stale_runtimes(state_root: &Path) {
        if let Ok(entries) = fs::read_dir(state_root) {
            for entry in entries.flatten() {
                let rt_dir = entry.path().join("runtime");
                if !rt_dir.is_dir() {
                    continue;
                }
                let pid_file = rt_dir.join(RUNTIME_PID_FILE);
                let meta_file = rt_dir.join(RUNTIME_META_FILE);
                let sock = rt_dir.join(RUNTIME_SOCKET_FILE);
                let spec_file = rt_dir.join(RUNTIME_SPEC_FILE);
                let cookie = crate::mode::read_private_regular(&spec_file)
                    .ok()
                    .and_then(|t| serde_json::from_str::<RoleRuntimeSpec>(&t).ok())
                    .map(|s| s.auth_cookie)
                    .unwrap_or_default();
                let meta = crate::mode::read_private_regular(&meta_file)
                    .ok()
                    .and_then(|t| serde_json::from_str::<RuntimeMeta>(&t).ok());
                let boot_now = current_boot_id();
                let my_pid = std::process::id();
                let my_pgrp = process_group_of(my_pid);
                // Fall back to `RuntimeMeta.pid` when `runtime.pid` was already removed
                // (e.g. clean self-exit on engine EOF) so orphaned confined descendants
                // are still swept before deleting the tombstone.
                let recorded_pid = fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|t| t.trim().parse::<u32>().ok())
                    .or_else(|| meta.as_ref().map(|m| m.pid));
                if let Some(pid) = recorded_pid
                    && pid > 1
                    && pid != my_pid
                    && Some(pid) != my_pgrp
                {
                    let same_boot = meta
                        .as_ref()
                        .zip(boot_now.as_deref())
                        .is_none_or(|(m, now)| m.boot_id == now);
                    if same_boot {
                        if process_is_role_runtime(pid) {
                            if !cookie.is_empty() {
                                send_shutdown_socket(&sock, &cookie);
                            }
                            let deadline = Instant::now() + Duration::from_secs(2);
                            while Instant::now() < deadline && process_is_role_runtime(pid) {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            if process_is_role_runtime(pid) {
                                crate::run::adapter_kill_orphan_runtime(pid);
                            } else {
                                crate::run::adapter_signal_run_group(pid, true);
                            }
                        } else if process_is_dead_or_zombie(pid)
                            && process_group_has_orphaned_members(
                                pid,
                                meta.as_ref().map(|m| m.start_time),
                            )
                        {
                            // Leader died (reaped or unreaped zombie) leaving live orphaned
                            // descendants in its process group (which Linux cannot reuse
                            // while any member survives): sweep the verified process group
                            // regardless of descendant argv.
                            crate::run::adapter_signal_run_group(pid, true);
                        }
                    }
                }
                let _ = fs::remove_file(&sock);
                let _ = fs::remove_file(&pid_file);
                let _ = fs::remove_file(&meta_file);
            }
        }
        // Clean up orphaned `/tmp/gewu-rt-<pid>-*` directories whose host PID is gone.
        if let Some(temp_root) = crate::confinement::resolved_temp()
            && let Ok(entries) = fs::read_dir(temp_root)
        {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name.strip_prefix("gewu-rt-") else {
                    continue;
                };
                let Some((pid_str, _)) = rest.split_once('-') else {
                    continue;
                };
                let Ok(pid) = pid_str.parse::<u32>() else {
                    continue;
                };
                if !process_is_role_runtime(pid) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }
}

impl Drop for HarnessPool {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn process_is_role_runtime(pid: u32) -> bool {
    let path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    text.contains("--role-runtime")
}

fn current_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn parse_proc_stat(pid: u32) -> Option<(char, u32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    // Fields after ')': 0=state, 1=ppid, 2=pgrp, ..., 19=starttime.
    let state = fields.next()?.chars().next()?;
    let pgrp = fields.nth(1)?.parse::<u32>().ok()?;
    let start_time = fields.nth(16)?.parse::<u64>().ok()?;
    Some((state, pgrp, start_time))
}

fn process_is_dead_or_zombie(pid: u32) -> bool {
    match parse_proc_stat(pid) {
        None => true,
        Some((state, _, _)) => matches!(state, 'Z' | 'X' | 'x'),
    }
}

fn process_group_of(pid: u32) -> Option<u32> {
    parse_proc_stat(pid).map(|(_, pgrp, _)| pgrp)
}

/// Check whether an orphaned process group `pgid` (whose leader PID is dead or a
/// zombie, preventing Linux from reusing `pgid` while any member survives) still has
/// any live same-UID member process (verified against `min_start_time` when available).
fn process_group_has_orphaned_members(pgid: u32, min_start_time: Option<u64>) -> bool {
    use std::os::unix::fs::MetadataExt;
    if pgid <= 1 {
        return false;
    }
    let my_uid = rustix::process::geteuid().as_raw();
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let proc_dir = entry.path();
        let Ok(meta) = fs::metadata(&proc_dir) else {
            continue;
        };
        if meta.uid() != my_uid {
            continue;
        }
        let Some((state, pgrp, start_time)) = parse_proc_stat(pid) else {
            continue;
        };
        if matches!(state, 'Z' | 'X' | 'x') {
            continue;
        }
        if pgrp != pgid {
            continue;
        }
        if min_start_time.is_some_and(|min_t| start_time < min_t) {
            continue;
        }
        return true;
    }
    false
}

fn connect_unix_safe(socket: &Path) -> std::io::Result<UnixStream> {
    if socket.as_os_str().len() <= 100 {
        return UnixStream::connect(socket);
    }
    if let (Some(parent), Some(name)) = (socket.parent(), socket.file_name())
        && Path::new("/proc/self/fd").is_dir()
    {
        use std::os::fd::AsRawFd;
        let dir_file = fs::File::open(parent)?;
        let proc_sock = PathBuf::from(format!("/proc/self/fd/{}", dir_file.as_raw_fd())).join(name);
        return UnixStream::connect(&proc_sock);
    }
    UnixStream::connect(socket)
}

fn bind_unix_safe(socket: &Path) -> std::io::Result<UnixListener> {
    if socket.as_os_str().len() <= 100 {
        return UnixListener::bind(socket);
    }
    if let (Some(parent), Some(name)) = (socket.parent(), socket.file_name())
        && Path::new("/proc/self/fd").is_dir()
    {
        use std::os::fd::AsRawFd;
        let dir_file = fs::File::open(parent)?;
        let proc_sock = PathBuf::from(format!("/proc/self/fd/{}", dir_file.as_raw_fd())).join(name);
        return UnixListener::bind(&proc_sock);
    }
    UnixListener::bind(socket)
}

/// Client helper: check if a runtime socket responds to an authenticated `Ping`.
pub fn ping_socket(socket: &Path, cookie: &str) -> bool {
    if cookie.is_empty() {
        return false;
    }
    let Ok(mut stream) = connect_unix_safe(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(req) = serde_json::to_string(&RuntimeRequest::Ping {
        cookie: cookie.to_owned(),
    }) else {
        return false;
    };
    if writeln!(stream, "{req}").is_err() || stream.flush().is_err() {
        return false;
    }
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).ok().unwrap_or(0) > 0
        && matches!(serde_json::from_str::<RuntimeFrame>(&line), Ok(RuntimeFrame::Pong))
}

fn send_shutdown_socket(socket: &Path, cookie: &str) {
    if cookie.is_empty() {
        return;
    }
    let Ok(mut stream) = connect_unix_safe(socket) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    if let Ok(req) = serde_json::to_string(&RuntimeRequest::Shutdown {
        cookie: cookie.to_owned(),
    }) {
        let _ = writeln!(stream, "{req}");
        let _ = stream.flush();
    }
}

/// One incoming item delivered by `execute_on_runtime`.
pub enum RuntimeOutput<'a> {
    Event(&'a str),
    Stderr(&'a str),
}

/// Client helper called by `--run-agent` (`agent_run/native.rs`) to execute one
/// stateless wakeup on its role's resident `RoleRuntimeHost`.
#[allow(clippy::too_many_arguments)]
pub fn execute_on_runtime(
    socket: &Path,
    cookie: &str,
    run_id: &str,
    run_dir: &Path,
    prompt: &str,
    timeout_secs: u64,
    ledger: &Path,
    counter: &Path,
    mut on_output: impl FnMut(RuntimeOutput<'_>),
) -> Result<i32, String> {
    if cookie.is_empty() {
        return Err("role runtime cookie is missing".to_owned());
    }
    let mut stream = connect_unix_safe(socket)
        .map_err(|e| format!("cannot connect to role runtime {}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(timeout_secs.saturating_add(30))));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let req = RuntimeRequest::Execute {
        cookie: cookie.to_owned(),
        run_id: run_id.to_owned(),
        run_dir: run_dir.to_path_buf(),
        prompt: prompt.to_owned(),
        timeout_secs,
        ledger: ledger.to_path_buf(),
        counter: counter.to_path_buf(),
    };
    let line = serde_json::to_string(&req)
        .map_err(|e| format!("cannot serialize runtime request: {e}"))?;
    writeln!(stream, "{line}")
        .and_then(|()| stream.flush())
        .map_err(|e| format!("cannot send runtime request: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .map_err(|e| format!("role runtime stream closed unexpectedly: {e}"))?;
        if n == 0 {
            return Err("role runtime closed the connection before completing the turn".to_owned());
        }
        let frame: RuntimeFrame = serde_json::from_str(buf.trim())
            .map_err(|e| format!("role runtime returned malformed frame: {e}"))?;
        match frame {
            RuntimeFrame::Event { line } => on_output(RuntimeOutput::Event(&line)),
            RuntimeFrame::Stderr { line } => on_output(RuntimeOutput::Stderr(&line)),
            RuntimeFrame::Done { exit_code } => return Ok(exit_code),
            RuntimeFrame::Error { message } => return Err(message),
            RuntimeFrame::Pong => {}
        }
    }
}

/// Entrypoint for `gewu-multiverse --role-runtime <spec-file>`.
pub fn serve_role_runtime(spec_path: &Path) -> i32 {
    let text = match fs::read_to_string(spec_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gewu-multiverse: role runtime spec cannot be read: {e}");
            return 1;
        }
    };
    let spec: RoleRuntimeSpec = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gewu-multiverse: role runtime spec is invalid: {e}");
            return 1;
        }
    };
    match run_role_runtime_host(&spec) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gewu-multiverse: role runtime failed: {e}");
            1
        }
    }
}

fn run_role_runtime_host(spec: &RoleRuntimeSpec) -> Result<(), String> {
    let program = Program::parse(&spec.harness)
        .ok_or_else(|| format!("unknown native harness `{}`", spec.harness))?;
    let protocol = Protocol::parse(&spec.protocol)
        .ok_or_else(|| format!("unknown protocol `{}`", spec.protocol))?;
    if !crate::confinement::landlock_enforced() {
        return Err("role runtime requires enforced kernel Landlock".to_owned());
    }
    let engine_temp = crate::confinement::resolved_temp()
        .ok_or_else(|| "engine temporary root cannot be resolved".to_owned())?;
    let rt_root = engine_temp.join(format!("gewu-rt-{}-{}", std::process::id(), spec.agent));
    let _cleanup = TempDirGuard(rt_root.clone());

    let mut host = RoleRuntimeHost::boot(spec, program, protocol, &rt_root)?;

    if spec.auth_cookie.trim().is_empty() {
        return Err("role runtime spec is missing auth_cookie".to_owned());
    }
    let _ = fs::remove_file(&spec.socket);
    let listener = bind_unix_safe(&spec.socket)
        .map_err(|e| format!("cannot bind role runtime socket: {e}"))?;
    let _ = crate::mode::set_mode(&spec.socket, 0o600);
    let _ = listener.set_nonblocking(true);
    let _ = crate::mode::write_private_nofollow(
        &spec.pid_file,
        std::process::id().to_string().as_bytes(),
    );
    if let (Some(boot_id), Some((_, _, start_time))) = (
        current_boot_id(),
        parse_proc_stat(std::process::id()),
    ) && let Some(parent) = spec.pid_file.parent()
    {
        let meta = RuntimeMeta {
            pid: std::process::id(),
            boot_id,
            start_time,
        };
        if let Ok(bytes) = serde_json::to_vec(&meta) {
            let _ = crate::mode::write_private_nofollow(parent.join(RUNTIME_META_FILE), &bytes);
        }
    }

    let my_uid = rustix::process::geteuid();
    let idle_limit = Duration::from_secs(spec.idle_secs.clamp(30, 3600));
    let mut last_activity = Instant::now();

    // Watch our inherited stdin keepalive pipe: EOF means the Engine process exited
    // (even via SIGKILL) or explicitly dropped this role's PoolEntry.
    let parent_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = std::sync::Arc::clone(&parent_gone);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 16];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
    }

    loop {
        if parent_gone.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                match rustix::net::sockopt::get_socket_peercred(&stream) {
                    Ok(cred) if cred.uid == my_uid => {}
                    _ => continue,
                }
                let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let Ok(req) = serde_json::from_str::<RuntimeRequest>(line.trim()) else {
                    let _ = send_frame(
                        &mut stream,
                        &RuntimeFrame::Error {
                            message: "invalid role runtime request".to_owned(),
                        },
                    );
                    continue;
                };
                // Constant-time per-boot 256-bit cookie verification BEFORE any action
                // so no same-UID confined agent shell can spoof Ping, Execute, or Shutdown.
                if !crate::mode::constant_time_eq(
                    req.cookie().as_bytes(),
                    spec.auth_cookie.as_bytes(),
                ) {
                    let _ = send_frame(
                        &mut stream,
                        &RuntimeFrame::Error {
                            message: "unauthorized role runtime request".to_owned(),
                        },
                    );
                    continue;
                }
                match req {
                    RuntimeRequest::Ping { .. } => {
                        let _ = send_frame(&mut stream, &RuntimeFrame::Pong);
                    }
                    RuntimeRequest::Shutdown { .. } => {
                        break;
                    }
                    RuntimeRequest::Execute {
                        run_id,
                        run_dir,
                        prompt,
                        timeout_secs,
                        ledger,
                        counter,
                        ..
                    } => {
                        host.handle_execute(
                            &mut stream,
                            &run_id,
                            &run_dir,
                            &prompt,
                            Duration::from_secs(timeout_secs.max(5)),
                            &ledger,
                            &counter,
                        );
                        last_activity = Instant::now();
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_activity.elapsed() >= idle_limit {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }

    host.shutdown();
    let _ = fs::remove_file(&spec.socket);
    let _ = fs::remove_file(&spec.pid_file);
    Ok(())
}

fn send_frame(stream: &mut UnixStream, frame: &RuntimeFrame) -> bool {
    let Ok(line) = serde_json::to_string(frame) else {
        return false;
    };
    writeln!(stream, "{line}").is_ok() && stream.flush().is_ok()
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Per-role resident host holding supervisor helpers + one confined harness driver.
struct RoleRuntimeHost {
    spec: RoleRuntimeSpec,
    program: Program,
    protocol: Protocol,
    exe: PathBuf,
    /// Canonicalized non-writable runtime root (`/tmp/gewu-rt-<pid>-<agent>`).
    /// NEVER granted writable in Landlock so a confined harness can never `rmdir`,
    /// `rename`, or replace its leaf children (`home`, `tmp`, `xdg`, `<program>`)
    /// with symlinks.
    rt_root: PathBuf,
    /// Supervisor-owned directory granted ONLY read-only in Landlock for THIS role
    /// (`model.key`, `mcp.cookie`, `.gitconfig`, `--confined-program` specs, overlays).
    /// Neither this role nor any sibling role can modify or symlink files here.
    ro_dir: PathBuf,
    /// Leaf writable directories directly under non-writable `rt_root`.
    home: PathBuf,
    tmp: PathBuf,
    config: PathBuf,
    /// Canonicalized role workspace and harness install root.
    workspace: PathBuf,
    install_root: PathBuf,
    /// Held in supervisor memory so DSH respawns never read a key back from disk.
    model_secret: Secret,
    key_file: PathBuf,
    git_config: PathBuf,
    skills: Vec<PathBuf>,
    external_env: BTreeMap<String, String>,
    external_secrets: BTreeMap<String, Secret>,
    role_mcp_servers: Vec<ExternalMcpServer>,
    redactor: Secret,
    mcp_state: Option<PathBuf>,
    mcp_socket: Option<PathBuf>,
    mcp_cookie: Option<PathBuf>,
    _mcp_child: Option<ChildGuard>,
    _git_proxy: Option<crate::git_proxy::GitProxy>,
    _model_proxy: Option<crate::model_proxy::ModelProxy>,
    model_url: Option<String>,
    model_catalog: Option<PathBuf>,
    driver: Option<Box<dyn ResidentDriver>>,
}

trait ResidentDriver {
    fn is_alive(&mut self) -> bool;
    fn run_ephemeral_turn(
        &mut self,
        run_id: &str,
        prompt: &str,
        timeout: Duration,
        redactor: &Secret,
        stream: &mut UnixStream,
    ) -> Result<i32, String>;
    fn shutdown(&mut self);
}

impl RoleRuntimeHost {
    fn boot(
        spec: &RoleRuntimeSpec,
        program: Program,
        protocol: Protocol,
        rt_root: &Path,
    ) -> Result<Self, String> {
        fs::create_dir_all(rt_root)
            .map_err(|e| format!("{} cannot be created: {e}", rt_root.display()))?;
        let _ = crate::mode::set_mode(rt_root, crate::mode::PRIVATE_DIR_MODE);
        let rt_root = fs::canonicalize(rt_root)
            .map_err(|e| format!("{} cannot be resolved: {e}", rt_root.display()))?;
        let ws_meta = fs::symlink_metadata(&spec.workspace)
            .map_err(|e| format!("workspace {} cannot be inspected: {e}", spec.workspace.display()))?;
        if ws_meta.file_type().is_symlink() || !ws_meta.file_type().is_dir() {
            return Err("role workspace must be a regular directory (not a symlink)".to_owned());
        }
        let workspace = fs::canonicalize(&spec.workspace)
            .map_err(|e| format!("workspace {} cannot be resolved: {e}", spec.workspace.display()))?;
        let install_root = fs::canonicalize(&spec.install_root).unwrap_or_else(|_| spec.install_root.clone());

        let host_dir = rt_root.join("host");
        let ro_dir = rt_root.join("ro");
        let home = rt_root.join("home");
        let tmp = rt_root.join("tmp");
        let config = rt_root.join("xdg");
        for d in [&host_dir, &ro_dir, &home, &tmp, &config] {
            crate::mode::ensure_dir_nofollow_under(&rt_root, d)
                .map_err(|e| format!("{} cannot be created: {e}", d.display()))?;
        }
        for sub in ["config", "state", "cache", "run", "data"] {
            let d = config.join(sub);
            crate::mode::ensure_dir_nofollow_under(&rt_root, &d)
                .map_err(|e| format!("{} cannot be created: {e}", d.display()))?;
        }

        let model_secret = credentials::model_key(&spec.credentials, &spec.agent)
            .map_err(|e| format!("model key cannot be read: {e}"))?;
        let redactor = model_secret
            .clone()
            .with_extra_redactions(credentials::all_secret_values(&spec.credentials));
        let key_file = ro_dir.join("model.key");
        crate::mode::write_private_nofollow_under(&rt_root, &key_file, model_secret.expose().as_bytes())
            .map_err(|e| format!("runtime model key cannot be written: {e}"))?;
        let git_config = ro_dir.join(".gitconfig");
        crate::mode::write_private_nofollow_under(&rt_root, &git_config, b"")
            .map_err(|e| format!("runtime .gitconfig cannot be initialized: {e}"))?;

        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|e| format!("product executable cannot be found: {e}"))?;

        let mut external_env = BTreeMap::new();
        for (k, v) in &spec.role_env {
            crate::engine::settings::validate_external_env_key(k)?;
            external_env.insert(k.clone(), v.clone());
        }
        // Fail CLOSED if credentials.toml contains any forbidden env secret key (e.g. HOME, LD_PRELOAD).
        let external_secrets = credentials::agent_env_secrets(&spec.credentials, &spec.agent)
            .map_err(|e| format!("role external secrets invalid: {e}"))?;
        for k in external_secrets.keys() {
            crate::engine::settings::validate_external_env_key(k)?;
        }
        let role_mcp_servers = match spec.role_mcp_config.as_deref() {
            Some(path) => role_mcp::load_role_mcp_servers(path, &external_env, &external_secrets)?,
            None => Vec::new(),
        };

        let mut skills = Vec::new();
        if let Ok(entries) = fs::read_dir(&spec.skills_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    skills.push(entry.path());
                }
            }
        }
        skills.sort();

        // Start supervisor-owned Forgejo MCP server + GitProxy when Forgejo is configured.
        let mut mcp_state = None;
        let mut mcp_socket = None;
        let mut mcp_cookie = None;
        let mut mcp_child = None;
        let mut git_proxy = None;

        if let Some(fj) = spec.forgejo.as_ref()
            && credentials::forgejo_agent_token(&spec.credentials, &spec.agent).is_ok()
        {
            crate::run::write_forgejo_context(&host_dir, fj)
                .map_err(|e| format!("runtime forgejo context cannot be written: {e}"))?;
            let role_stub = crate::run::RoleRunPlan {
                permissions: Some(spec.permissions.clone()),
                ..crate::run::RoleRunPlan::default()
            };
            let _ = role_stub.write(&host_dir);

            let state_dir = host_dir.join("mcp-state");
            fs::create_dir_all(&state_dir)
                .map_err(|e| format!("runtime MCP state dir cannot be created: {e}"))?;
            let _ = crate::mode::set_mode(&state_dir, crate::mode::PRIVATE_DIR_MODE);
            let cookie_val = crate::mode::random_hex_256()
                .map_err(|e| format!("runtime MCP cookie cannot be generated: {e}"))?;
            crate::mode::write_private_nofollow(
                state_dir.join(crate::mcp_forgejo::MCP_COOKIE_FILE),
                cookie_val.as_bytes(),
            )
            .map_err(|e| format!("runtime MCP cookie cannot be written: {e}"))?;
            let ro_cookie = ro_dir.join(crate::mcp_forgejo::MCP_COOKIE_FILE);
            crate::mode::write_private_nofollow(&ro_cookie, cookie_val.as_bytes())
                .map_err(|e| format!("runtime role MCP cookie cannot be written: {e}"))?;
            let sock = state_dir.join("mcp.sock");
            let _ = fs::remove_file(&sock);

            let child = Command::new(&exe)
                .arg("--mcp-forgejo-serve")
                .arg(&sock)
                .arg(&state_dir)
                .arg(&host_dir)
                .arg(&spec.credentials)
                .arg(&spec.agent)
                .arg(&spec.workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("runtime Forgejo MCP server cannot start: {e}"))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !sock.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !sock.exists() {
                let mut c = child;
                let _ = c.kill();
                let _ = c.wait();
                return Err("runtime Forgejo MCP server did not become ready".to_owned());
            }
            mcp_state = Some(state_dir);
            mcp_socket = Some(sock);
            mcp_cookie = Some(ro_cookie);
            mcp_child = Some(ChildGuard(child));

            git_proxy =
                crate::git_proxy::GitProxy::start(&host_dir, &spec.agent, &spec.credentials)?;
            if let Some(proxy) = git_proxy.as_ref() {
                let text = format!("[http]\n\tproxy = {}\n", proxy.url());
                crate::mode::write_private_nofollow(&git_config, text.as_bytes())
                    .map_err(|e| format!("runtime .gitconfig cannot be written: {e}"))?;
            }
        }

        let mut model_proxy = None;
        let mut model_url = None;
        let mut model_catalog = None;
        if program == Program::Codex && protocol == Protocol::Responses {
            let proxy = crate::model_proxy::ModelProxy::start(
                &spec.base_url,
                Duration::from_secs(spec.timeout_secs.max(30)),
                crate::model_proxy::IDLE_TIMEOUT,
            )?;
            model_url = Some(proxy.url());
            model_proxy = Some(proxy);
            model_catalog = write_codex_catalog(&ro_dir, &home, &spec.program, &spec.model)?;
        }

        let mut host = Self {
            spec: spec.clone(),
            program,
            protocol,
            exe,
            rt_root,
            ro_dir,
            home,
            tmp,
            config,
            workspace,
            install_root,
            model_secret,
            key_file,
            git_config,
            skills,
            external_env,
            external_secrets,
            role_mcp_servers,
            redactor,
            mcp_state,
            mcp_socket,
            mcp_cookie,
            _mcp_child: mcp_child,
            _git_proxy: git_proxy,
            _model_proxy: model_proxy,
            model_url,
            model_catalog,
            driver: None,
        };
        host.ensure_driver()?;
        Ok(host)
    }

    fn ensure_driver(&mut self) -> Result<(), String> {
        if let Some(driver) = self.driver.as_mut()
            && driver.is_alive()
        {
            return Ok(());
        }
        self.recycle_driver();
        let driver: Box<dyn ResidentDriver> = match self.program {
            Program::Dsh => Box::new(DshSdkDriver::spawn(self)?),
            Program::Pi => Box::new(PiRpcDriver::spawn(self)?),
            Program::Codex => Box::new(CodexAppServerDriver::spawn(self)?),
            Program::Opencode => Box::new(OpencodeServeDriver::spawn(self)?),
            Program::Builtin => {
                return Err("built-in engine does not use resident harness runtime".to_owned());
            }
        };
        self.driver = Some(driver);
        Ok(())
    }

    fn recycle_driver(&mut self) {
        if let Some(mut driver) = self.driver.take() {
            driver.shutdown();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_execute(
        &mut self,
        stream: &mut UnixStream,
        run_id: &str,
        run_dir: &Path,
        prompt: &str,
        timeout: Duration,
        ledger: &Path,
        counter: &Path,
    ) {
        // Strictly validate that `run_dir`, `ledger`, and `counter` live inside this
        // role's authoritative `<state-root>/<agent>/` tree with zero symlink escapes.
        let validated = match validate_run_paths(&self.spec.runs_root, run_dir, ledger, counter) {
            Ok(paths) => paths,
            Err(reason) => {
                let _ = send_frame(stream, &RuntimeFrame::Error { message: reason });
                return;
            }
        };
        // Point the persistent MCP bridge at this exact validated run's ledger/counter
        // using ONLY the immutable `self.spec.permissions` booted by the Engine.
        if let Some(state_dir) = self.mcp_state.as_deref() {
            let ptr = ActiveRunPointer {
                run_dir: validated.0,
                ledger: validated.1,
                counter: validated.2,
                permissions: self.spec.permissions.clone(),
            };
            if let Err(e) = ptr.write(state_dir) {
                let _ = send_frame(
                    stream,
                    &RuntimeFrame::Error {
                        message: format!("cannot switch MCP active run: {e}"),
                    },
                );
                return;
            }
        }
        if let Err(e) = self.ensure_driver() {
            let _ = send_frame(
                stream,
                &RuntimeFrame::Error {
                    message: self.redactor.redact(&e),
                },
            );
            return;
        }
        let redactor = self.redactor.clone();
        let result = match self.driver.as_mut() {
            Some(driver) => driver.run_ephemeral_turn(run_id, prompt, timeout, &redactor, stream),
            None => Err("resident harness driver unavailable".to_owned()),
        };
        match result {
            Ok(exit_code) => {
                let _ = send_frame(stream, &RuntimeFrame::Done { exit_code });
            }
            Err(e) => {
                // Always recycle the resident process after any error/timeout/disconnect
                // so zero dirty or in-flight state can survive into the next wakeup.
                self.recycle_driver();
                let _ = send_frame(
                    stream,
                    &RuntimeFrame::Error {
                        message: self.redactor.redact(&e),
                    },
                );
            }
        }
    }

    fn shutdown(&mut self) {
        self.recycle_driver();
    }

    fn base_confined_command(
        &self,
        spec_name: &str,
        program_path: &Path,
        args: &[String],
        extra_env: &[(String, String)],
        extra_write: &[&Path],
    ) -> Result<Command, String> {
        let mut read_paths = vec![
            self.install_root.display().to_string(),
            self.ro_dir.display().to_string(),
        ];
        if let Some(exe_parent) = self.exe.parent() {
            read_paths.push(exe_parent.display().to_string());
        }
        if let Some(sock_parent) = self.mcp_socket.as_ref().and_then(|s| s.parent()) {
            read_paths.push(sock_parent.display().to_string());
        }
        // Grant ONLY leaf directories directly under non-writable `self.rt_root`
        // (`home`, `tmp`, `xdg`, and the program's own state leaf) — NEVER `rt_root`
        // itself — so a confined harness can never rmdir/replace any of them with a symlink.
        let mut write_paths = vec![
            self.home.display().to_string(),
            self.tmp.display().to_string(),
            self.config.display().to_string(),
        ];
        for dir in extra_write {
            write_paths.push(dir.display().to_string());
        }
        let spec_file = self.ro_dir.join(spec_name);
        write_json_under(
            &self.rt_root,
            &spec_file,
            &json!({
                "workspace": self.workspace.display().to_string(),
                "program": program_path.display().to_string(),
                "args": args,
                "read": read_paths,
                "write": write_paths,
            }),
        )?;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--confined-program")
            .arg(&spec_file)
            .current_dir(&self.workspace);
        cmd.env_clear();
        for (k, v) in crate::confinement::safe_environment(std::env::vars_os()) {
            cmd.env(k, v);
        }
        cmd.env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("XDG_CONFIG_HOME", self.config.join("config"))
            .env("XDG_DATA_HOME", self.config.join("data"))
            .env("XDG_STATE_HOME", self.config.join("state"))
            .env("XDG_CACHE_HOME", self.config.join("cache"))
            .env("XDG_RUNTIME_DIR", self.config.join("run"))
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GEWU_AGENT", &self.spec.agent)
            .env("GEWU_WORKSPACE", &self.workspace);
        for (k, v) in &self.external_env {
            crate::engine::settings::validate_external_env_key(k)?;
            cmd.env(k, v);
        }
        for (k, secret) in &self.external_secrets {
            crate::engine::settings::validate_external_env_key(k)?;
            cmd.env(k, secret.expose());
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        Ok(cmd)
    }
}

/// Validate that `run_dir` is a real non-symlink directory beneath `runs_root`, and
/// that `ledger` and `counter` live directly under `<run_dir>/mcp-state/` without
/// any symlink traversal.
fn validate_run_paths(
    runs_root: &Path,
    run_dir: &Path,
    ledger: &Path,
    counter: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let canon_root = fs::canonicalize(runs_root)
        .map_err(|e| format!("role runs_root cannot be resolved: {e}"))?;
    let run_meta = fs::symlink_metadata(run_dir)
        .map_err(|e| format!("run_dir cannot be inspected: {e}"))?;
    if run_meta.file_type().is_symlink() || !run_meta.file_type().is_dir() {
        return Err("run_dir must be a regular directory (not a symlink)".to_owned());
    }
    let canon_run =
        fs::canonicalize(run_dir).map_err(|e| format!("run_dir cannot be resolved: {e}"))?;
    if !canon_run.starts_with(&canon_root) || canon_run == canon_root {
        return Err("run_dir resolves outside this role's state directory".to_owned());
    }
    let expected_mcp_state = canon_run.join("mcp-state");
    let mcp_meta = fs::symlink_metadata(&expected_mcp_state)
        .map_err(|e| format!("run mcp-state cannot be inspected: {e}"))?;
    if mcp_meta.file_type().is_symlink() || !mcp_meta.file_type().is_dir() {
        return Err("run mcp-state must be a regular directory".to_owned());
    }
    let canon_mcp = fs::canonicalize(&expected_mcp_state)
        .map_err(|e| format!("run mcp-state cannot be resolved: {e}"))?;
    if !canon_mcp.starts_with(&canon_run) {
        return Err("run mcp-state resolves outside run_dir".to_owned());
    }
    let check_child = |p: &Path, expected_name: &str| -> Result<PathBuf, String> {
        let parent = p
            .parent()
            .ok_or_else(|| format!("{} has no parent", p.display()))?;
        let canon_parent = fs::canonicalize(parent)
            .map_err(|e| format!("{} parent cannot be resolved: {e}", p.display()))?;
        if canon_parent != canon_mcp {
            return Err(format!(
                "{} must live directly inside <run_dir>/mcp-state",
                p.display()
            ));
        }
        if p.file_name().and_then(|n| n.to_str()) != Some(expected_name) {
            return Err(format!("unexpected MCP state filename {}", p.display()));
        }
        if let Ok(meta) = fs::symlink_metadata(p)
            && meta.file_type().is_symlink()
        {
            return Err(format!("{} must not be a symlink", p.display()));
        }
        Ok(canon_mcp.join(expected_name))
    };
    let clean_ledger = check_child(ledger, "calls.jsonl")?;
    let clean_counter = check_child(counter, "writes")?;
    Ok((canon_run, clean_ledger, clean_counter))
}

// ============================================================================
// 1. DeepSeek Harness (`dsh --profile sdk`) Resident Driver
// ============================================================================

struct DshSdkDriver {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<(bool, String)>,
    next_id: u64,
}

impl DshSdkDriver {
    fn spawn(host: &RoleRuntimeHost) -> Result<Self, String> {
        let dir = host.rt_root.join("dsh");
        crate::mode::ensure_dir_nofollow_under(&host.rt_root, &dir)
            .map_err(|e| format!("{} cannot be created: {e}", dir.display()))?;
        let api = match host.protocol {
            Protocol::ChatCompletions => "openai-completions",
            Protocol::Responses => "openai-responses",
            Protocol::AnthropicMessages => "anthropic-messages",
        };
        let (request_ms, retry_delay_ms) = launch_budget(host.spec.timeout_secs);
        let mut overlay = format!(
            "- id: agent-default-model\n  config:\n    provider: gewu\n    model: {}\n- id: llm-pi-ai\n  config:\n    providers:\n      gewu:\n        apiKeyEnv: GEWU_NATIVE_KEY\n        api: {}\n        baseURL: {}\n        timeoutMs: {}\n        streamIdleTimeoutMs: {}\n        retryPolicy:\n          mode: normal\n          maxRetries: 2\n          backoff:\n            initialDelayMs: {}\n            maxDelayMs: {}\n        models:\n          - id: {}\n- id: session-title-llm\n  disabled: true\n",
            yaml_esc(&host.spec.model),
            yaml_esc(api),
            yaml_esc(&host.spec.base_url),
            request_ms,
            request_ms,
            retry_delay_ms,
            retry_delay_ms.saturating_mul(4),
            yaml_esc(&host.spec.model),
        );
        if let Some(socket) = host.mcp_socket.as_ref() {
            let exe_str = host.exe.display().to_string();
            let sock_str = socket.display().to_string();
            let mcp_args = match host.mcp_cookie.as_ref() {
                Some(cookie) => format!(
                    "[{}, {}, {}]",
                    yaml_esc("--mcp-forgejo-connect"),
                    yaml_esc(&sock_str),
                    yaml_esc(&cookie.display().to_string()),
                ),
                None => format!(
                    "[{}, {}]",
                    yaml_esc("--mcp-forgejo-connect"),
                    yaml_esc(&sock_str),
                ),
            };
            overlay.push_str(&format!(
                "- insert:\n    - id: gewu-forgejo\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: gewu_forgejo\n        transport: stdio\n        command: {}\n        args: {}\n        failOnStartupError: true\n",
                yaml_esc(&exe_str),
                mcp_args,
            ));
        }
        if !host.role_mcp_servers.is_empty() {
            overlay.push_str(&role_mcp::render_dsh_overlay_servers(&host.role_mcp_servers));
        }
        // Write overlay.yml into Landlock read-only `ro_dir` (never harness-writable `dir`)
        // and read the model key strictly from supervisor memory (never back from disk).
        let overlay_path = host.ro_dir.join("dsh-overlay.yml");
        crate::mode::write_private_nofollow_under(&host.rt_root, &overlay_path, overlay.as_bytes())
            .map_err(|e| format!("DSH overlay.yml cannot be written: {e}"))?;
        crate::mode::write_private_nofollow_under(
            &host.rt_root,
            dir.join(".env"),
            format!("GEWU_NATIVE_KEY={}\n", host.model_secret.expose().trim_end()).as_bytes(),
        )
        .map_err(|e| format!("DSH .env cannot be written: {e}"))?;
        for skill in &host.skills {
            if let Some(name) = skill.file_name() {
                copy_tree_under(&host.rt_root, skill, &dir.join("skills").join(name))?;
            }
        }
        let args = vec![
            "--profile".to_owned(),
            "sdk".to_owned(),
            "--patch".to_owned(),
            overlay_path.display().to_string(),
        ];
        let envs = vec![
            ("DSH_HOME".to_owned(), dir.display().to_string()),
            ("DSH_PERMISSION_MODE".to_owned(), "workspace-write".to_owned()),
        ];
        let mut cmd =
            host.base_confined_command("dsh-sdk.json", &host.spec.program, &args, &envs, &[&dir])?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("DSH SDK resident server failed to start: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "DSH SDK stdin unavailable".to_owned())?;
        let rx = spawn_stdio_readers(&mut child);
        let mut driver = Self {
            child,
            stdin,
            rx,
            next_id: 1,
        };
        // Perform SDK handshake once.
        let init_id = driver.send_rpc(
            "initialize",
            json!({
                "cwd": host.spec.workspace.display().to_string(),
                "provider": "gewu",
                "model": host.spec.model,
            }),
        )?;
        driver.wait_rpc_response(init_id, Duration::from_secs(20), &mut |_| {})?;
        Ok(driver)
    }

    fn send_rpc(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{frame}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("DSH SDK write failed: {e}"))?;
        Ok(id)
    }

    fn wait_rpc_response(
        &mut self,
        target_id: u64,
        timeout: Duration,
        on_msg: &mut impl FnMut(RpcSideMsg<'_>),
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("DSH SDK request {target_id} timed out"));
            }
            match self.rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok((true, line)) => {
                    let Ok(val) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if val.get("id").and_then(Value::as_u64) == Some(target_id) {
                        if let Some(err) = val.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("DSH SDK error");
                            return Err(msg.to_owned());
                        }
                        return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if val.get("method").is_some() {
                        on_msg(RpcSideMsg::Notification(&val));
                    }
                }
                Ok((false, line)) => {
                    on_msg(RpcSideMsg::Stderr(&line));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err("DSH SDK resident process exited unexpectedly".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("DSH SDK stdout closed unexpectedly".to_owned());
                }
            }
        }
    }
}

enum RpcSideMsg<'a> {
    Notification(&'a Value),
    Stderr(&'a str),
}

impl ResidentDriver for DshSdkDriver {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn run_ephemeral_turn(
        &mut self,
        run_id: &str,
        prompt: &str,
        timeout: Duration,
        redactor: &Secret,
        stream: &mut UnixStream,
    ) -> Result<i32, String> {
        let mut saw_terminal_error = false;
        let mut client_broken = false;
        let session_id = run_id.to_owned();

        let prompt_id = self.send_rpc(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "contentBlocks": [{"type": "text", "text": prompt}],
            }),
        )?;

        let handle = |msg: RpcSideMsg<'_>,
                      session_id: &str,
                      redactor: &Secret,
                      stream: &mut UnixStream,
                      client_broken: &mut bool,
                      saw_terminal_error: &mut bool| match msg {
            RpcSideMsg::Notification(val) => {
                for mapped in translate_dsh_sdk_notification(val, session_id) {
                    if mapped.get("type").and_then(Value::as_str) == Some("status")
                        && mapped
                            .get("reason")
                            .and_then(|r| r.get("kind"))
                            .and_then(Value::as_str)
                            != Some("completed")
                    {
                        *saw_terminal_error = true;
                    }
                    let line = redactor.redact(&mapped.to_string());
                    if !send_frame(stream, &RuntimeFrame::Event { line }) {
                        *client_broken = true;
                    }
                }
            }
            RpcSideMsg::Stderr(err_line) => {
                let cleaned = redactor.redact(err_line);
                if !cleaned.trim().is_empty()
                    && !send_frame(stream, &RuntimeFrame::Stderr { line: cleaned })
                {
                    *client_broken = true;
                }
            }
        };

        self.wait_rpc_response(
            prompt_id,
            timeout.min(Duration::from_secs(30)),
            &mut |msg| {
                handle(
                    msg,
                    &session_id,
                    redactor,
                    stream,
                    &mut client_broken,
                    &mut saw_terminal_error,
                )
            },
        )?;
        if client_broken {
            return Err("client disconnected during DSH prompt".to_owned());
        }

        let wait_id = self.send_rpc("session/wait", json!({ "sessionId": session_id }))?;
        self.wait_rpc_response(wait_id, timeout, &mut |msg| {
            handle(
                msg,
                &session_id,
                redactor,
                stream,
                &mut client_broken,
                &mut saw_terminal_error,
            )
        })?;
        if client_broken {
            return Err("client disconnected during DSH wait".to_owned());
        }
        Ok(if saw_terminal_error { 1 } else { 0 })
    }

    fn shutdown(&mut self) {
        let _ = self.send_rpc("shutdown", json!({}));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !self.is_alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DshSdkDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Translate one DSH SDK `session.event` JSON-RPC notification into canonical
/// DSH headless JSON events understood by `agent_run/native/outcome.rs`.
pub(crate) fn translate_dsh_sdk_notification(val: &Value, expected_session: &str) -> Vec<Value> {
    if val.get("method").and_then(Value::as_str) != Some("session.event") {
        return Vec::new();
    }
    let Some(params) = val.get("params") else {
        return Vec::new();
    };
    if params.get("sessionId").and_then(Value::as_str) != Some(expected_session) {
        return Vec::new();
    }
    let Some(ev) = params.get("event") else {
        return Vec::new();
    };
    let ev_type = ev.get("type").and_then(Value::as_str).unwrap_or_default();
    let data = ev.get("data").unwrap_or(&Value::Null);
    match ev_type {
        "tool/call" => {
            let call_id = data.get("callId").and_then(Value::as_str).unwrap_or_default();
            let name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
            let raw_args = data
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(raw_args)
                .unwrap_or_else(|_| Value::String(raw_args.to_owned()));
            vec![json!({
                "type": "tool_call",
                "callId": call_id,
                "tool": name,
                "input": input,
            })]
        }
        "tool/result" => {
            let msg = data.get("message").unwrap_or(&Value::Null);
            let call_id = msg
                .get("toolCallId")
                .or_else(|| msg.get("callId"))
                .or_else(|| data.get("callId"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false)
                || data.get("error").is_some();
            let mut text = extract_content_text(msg.get("content"));
            if text.is_empty()
                && let Some(reason) = data
                    .get("error")
                    .and_then(|e| e.get("reason").or_else(|| e.get("name")))
                    .and_then(Value::as_str)
            {
                text = reason.to_owned();
            }
            vec![json!({
                "type": "tool_result",
                "callId": call_id,
                "status": if is_error { "error" } else { "completed" },
                "result": text,
            })]
        }
        "assistant/message" => {
            let msg = data.get("message").unwrap_or(&Value::Null);
            let text = extract_content_text(msg.get("content"));
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "type": "text",
                    "text": text,
                })]
            }
        }
        "turn/end" => {
            let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(1);
            let reason = data.get("reason").cloned().unwrap_or_else(|| json!({"kind": "completed"}));
            vec![json!({
                "type": "status",
                "phase": "turn_end",
                "turn": turn,
                "reason": reason,
            })]
        }
        _ => Vec::new(),
    }
}

fn extract_content_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for item in arr {
        if let Some(t) = item.get("text").and_then(Value::as_str) {
            out.push_str(t);
        }
    }
    out
}

// ============================================================================
// 2. Pi (`pi --mode rpc --no-session`) Resident Driver
// ============================================================================

struct PiRpcDriver {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<(bool, String)>,
}

impl PiRpcDriver {
    fn spawn(host: &RoleRuntimeHost) -> Result<Self, String> {
        let dir = host.rt_root.join("pi");
        crate::mode::ensure_dir_nofollow_under(&host.rt_root, &dir)
            .map_err(|e| format!("{} cannot be created: {e}", dir.display()))?;
        let api = match host.protocol {
            Protocol::ChatCompletions => "openai-completions",
            Protocol::Responses => "openai-responses",
            Protocol::AnthropicMessages => "anthropic-messages",
        };
        let (request_ms, retry_delay_ms) = launch_budget(host.spec.timeout_secs);
        write_json_under(
            &host.rt_root,
            &dir.join("models.json"),
            &json!({
                "providers": {
                    "gewu": {
                        "baseUrl": host.spec.base_url,
                        "api": api,
                        "apiKey": format!("!{} --print-native-key {}", host.exe.display(), host.key_file.display()),
                        "models": [{"id": host.spec.model, "name": host.spec.model}]
                    }
                }
            }),
        )?;
        write_json_under(
            &host.rt_root,
            &dir.join("settings.json"),
            &json!({
                "httpIdleTimeoutMs": request_ms,
                "retry": { "maxRetries": 2, "baseDelayMs": retry_delay_ms }
            }),
        )?;
        let mut servers = serde_json::Map::new();
        if let Some(socket) = host.mcp_socket.as_ref() {
            let mut mcp_args = vec![
                "--mcp-forgejo-connect".to_owned(),
                socket.display().to_string(),
            ];
            if let Some(cookie) = host.mcp_cookie.as_ref() {
                mcp_args.push(cookie.display().to_string());
            }
            servers.insert(
                "gewu_forgejo".to_owned(),
                json!({
                    "command": host.exe.display().to_string(),
                    "args": mcp_args,
                    "exposure": "direct",
                    "description": "gewu-multiverse Forgejo tools for this agent"
                }),
            );
        }
        role_mcp::insert_pi_servers(&mut servers, &host.role_mcp_servers);
        if !servers.is_empty() {
            write_json_under(
                &host.rt_root,
                &dir.join("mcp.json"),
                &json!({ "mcpServers": servers }),
            )?;
        }

        let mut args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            "--no-session".to_owned(),
            "--provider".to_owned(),
            "gewu".to_owned(),
            "--model".to_owned(),
            host.spec.model.clone(),
        ];
        let skills_dir = host.ro_dir.join("pi-skills");
        for skill in &host.skills {
            if let Some(name) = skill.file_name() {
                copy_tree_under(&host.rt_root, skill, &skills_dir.join(name))?;
                args.push("--skill".to_owned());
                args.push(skills_dir.join(name).display().to_string());
            }
        }
        let envs = vec![
            ("PI_CODING_AGENT_DIR".to_owned(), dir.display().to_string()),
            ("PI_OFFLINE".to_owned(), "1".to_owned()),
        ];
        let mut cmd =
            host.base_confined_command("pi-rpc.json", &host.spec.program, &args, &envs, &[&dir])?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Pi RPC resident process failed to start: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Pi RPC stdin unavailable".to_owned())?;
        let rx = spawn_stdio_readers(&mut child);
        Ok(Self { child, stdin, rx })
    }
}

impl ResidentDriver for PiRpcDriver {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn run_ephemeral_turn(
        &mut self,
        run_id: &str,
        prompt: &str,
        timeout: Duration,
        redactor: &Secret,
        stream: &mut UnixStream,
    ) -> Result<i32, String> {
        let deadline = Instant::now() + timeout;
        // 1. Reset to a completely fresh ephemeral session before every wakeup.
        let new_id = format!("new-{run_id}");
        let new_cmd = json!({ "id": new_id, "type": "new_session" });
        writeln!(self.stdin, "{new_cmd}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("Pi RPC new_session write failed: {e}"))?;

        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err("Pi RPC new_session timed out".to_owned());
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(200))) {
                Ok((true, line)) => {
                    let Ok(val) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if val.get("type").and_then(Value::as_str) == Some("response")
                        && val.get("id").and_then(Value::as_str) == Some(new_id.as_str())
                    {
                        if val.get("success").and_then(Value::as_bool) != Some(true) {
                            let err = val
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("new_session failed");
                            return Err(format!("Pi RPC new_session refused: {err}"));
                        }
                        break;
                    }
                }
                Ok((false, line)) => {
                    let cleaned = redactor.redact(&line);
                    if !cleaned.trim().is_empty() {
                        let _ = send_frame(stream, &RuntimeFrame::Stderr { line: cleaned });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err("Pi RPC process exited during new_session".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Pi RPC closed during new_session".to_owned());
                }
            }
        }

        // 2. Dispatch prompt and stream verbatim Pi JSON events until `agent_settled`.
        let prompt_id = format!("prompt-{run_id}");
        let prompt_cmd = json!({
            "id": prompt_id,
            "type": "prompt",
            "message": prompt,
        });
        writeln!(self.stdin, "{prompt_cmd}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("Pi RPC prompt write failed: {e}"))?;

        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err("Pi RPC turn timed out".to_owned());
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(200))) {
                Ok((true, line)) => {
                    let Ok(val) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if val.get("type").and_then(Value::as_str) == Some("response")
                        && val.get("id").and_then(Value::as_str) == Some(prompt_id.as_str())
                    {
                        if val.get("success").and_then(Value::as_bool) != Some(true) {
                            let err = val
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("prompt rejected");
                            return Err(format!("Pi RPC prompt failed: {err}"));
                        }
                        if val
                            .get("data")
                            .and_then(|d| d.get("disposition"))
                            .and_then(Value::as_str)
                            == Some("handled")
                        {
                            return Ok(0);
                        }
                        continue;
                    }
                    let is_settled =
                        val.get("type").and_then(Value::as_str) == Some("agent_settled");
                    let cleaned = redactor.redact(&line);
                    if !send_frame(stream, &RuntimeFrame::Event { line: cleaned }) {
                        return Err("client disconnected during Pi turn".to_owned());
                    }
                    if is_settled {
                        return Ok(0);
                    }
                }
                Ok((false, line)) => {
                    let cleaned = redactor.redact(&line);
                    if !cleaned.trim().is_empty()
                        && !send_frame(stream, &RuntimeFrame::Stderr { line: cleaned })
                    {
                        return Err("client disconnected during Pi stderr".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err("Pi RPC process exited mid-turn".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Pi RPC closed mid-turn".to_owned());
                }
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PiRpcDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ============================================================================
// 3. Codex (`codex app-server --listen stdio://`) Resident Driver
// ============================================================================

struct CodexAppServerDriver {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<(bool, String)>,
    next_id: u64,
    workspace: String,
    model: String,
    sandbox: String,
}

impl CodexAppServerDriver {
    fn spawn(host: &RoleRuntimeHost) -> Result<Self, String> {
        let dir = host.rt_root.join("codex");
        crate::mode::ensure_dir_nofollow_under(&host.rt_root, &dir)
            .map_err(|e| format!("{} cannot be created: {e}", dir.display()))?;
        let sandbox = if host.spec.codex_sandbox == "full-access" {
            "danger-full-access".to_owned()
        } else {
            "workspace-write".to_owned()
        };
        let (request_ms, _) = launch_budget(host.spec.timeout_secs);
        let base_url = host.model_url.as_deref().unwrap_or(&host.spec.base_url);
        let catalog = host
            .model_catalog
            .as_ref()
            .map(|p| format!("model_catalog_json = {}\n", toml_esc(&p.display().to_string())))
            .unwrap_or_default();
        let mut config = format!(
            "{}model = {}\nmodel_provider = \"gewu\"\napproval_policy = \"never\"\nsandbox_mode = {}\nweb_search = \"disabled\"\n\n[sandbox_workspace_write]\nnetwork_access = true\n\n[features]\nplugins = false\nrecommended_plugins = false\nmulti_agent = false\n\n[model_providers.gewu]\nname = \"gewu\"\nbase_url = {}\nwire_api = \"responses\"\nrequest_max_retries = 2\nstream_max_retries = 2\nstream_idle_timeout_ms = {}\n\n[model_providers.gewu.auth]\ncommand = {}\nargs = [\"--print-native-key\", {}]\n",
            catalog,
            toml_esc(&host.spec.model),
            toml_esc(&sandbox),
            toml_esc(base_url),
            request_ms,
            toml_esc(&host.exe.display().to_string()),
            toml_esc(&host.key_file.display().to_string()),
        );
        if let Some(socket) = host.mcp_socket.as_ref() {
            let mcp_args = match host.mcp_cookie.as_ref() {
                Some(cookie) => format!(
                    "[\"--mcp-forgejo-connect\", {}, {}]",
                    toml_esc(&socket.display().to_string()),
                    toml_esc(&cookie.display().to_string()),
                ),
                None => format!(
                    "[\"--mcp-forgejo-connect\", {}]",
                    toml_esc(&socket.display().to_string()),
                ),
            };
            config.push_str(&format!(
                "\n[mcp_servers.gewu_forgejo]\ncommand = {}\nargs = {}\nrequired = true\nstartup_timeout_sec = 20\ntool_timeout_sec = 120\n",
                toml_esc(&host.exe.display().to_string()),
                mcp_args,
            ));
            for tool in crate::forgejo_tools::allowed_tool_names(&host.spec.permissions) {
                config.push_str(&format!(
                    "\n[mcp_servers.gewu_forgejo.tools.{tool}]\napproval_mode = \"approve\"\n"
                ));
            }
        }
        if !host.role_mcp_servers.is_empty() {
            config.push_str(&role_mcp::render_codex_toml_servers(&host.role_mcp_servers));
        }
        crate::mode::write_private_nofollow_under(&host.rt_root, dir.join("config.toml"), config.as_bytes())
            .map_err(|e| format!("Codex config.toml cannot be written: {e}"))?;
        for skill in &host.skills {
            if let Some(name) = skill.file_name() {
                copy_tree_under(&host.rt_root, skill, &dir.join("skills").join(name))?;
            }
        }
        let args = vec![
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned(),
        ];
        let envs = vec![
            ("CODEX_HOME".to_owned(), dir.display().to_string()),
            (
                "RUST_LOG".to_owned(),
                "codex_core=warn,codex_otel=info".to_owned(),
            ),
        ];
        let mut cmd = host.base_confined_command(
            "codex-app-server.json",
            &host.spec.program,
            &args,
            &envs,
            &[&dir],
        )?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Codex app-server failed to start: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Codex app-server stdin unavailable".to_owned())?;
        let rx = spawn_stdio_readers(&mut child);
        let mut driver = Self {
            child,
            stdin,
            rx,
            next_id: 1,
            workspace: host.spec.workspace.display().to_string(),
            model: host.spec.model.clone(),
            sandbox,
        };
        let init_id = driver.send_rpc(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "gewu-multiverse",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )?;
        driver.wait_rpc(init_id, Duration::from_secs(20), &mut |_| {})?;
        // Send initialized notification.
        let notif = json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} });
        let _ = writeln!(driver.stdin, "{notif}");
        let _ = driver.stdin.flush();
        Ok(driver)
    }

    fn send_rpc(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{frame}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("Codex app-server write failed: {e}"))?;
        Ok(id)
    }

    fn wait_rpc(
        &mut self,
        target_id: u64,
        timeout: Duration,
        on_msg: &mut impl FnMut(RpcSideMsg<'_>),
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(format!("Codex app-server request {target_id} timed out"));
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(200))) {
                Ok((true, line)) => {
                    let Ok(val) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if val.get("id").and_then(Value::as_u64) == Some(target_id) {
                        if let Some(err) = val.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("Codex app-server error");
                            return Err(msg.to_owned());
                        }
                        return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if val.get("method").is_some() {
                        on_msg(RpcSideMsg::Notification(&val));
                    }
                }
                Ok((false, line)) => on_msg(RpcSideMsg::Stderr(&line)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err("Codex app-server exited unexpectedly".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Codex app-server stdout closed unexpectedly".to_owned());
                }
            }
        }
    }
}

impl ResidentDriver for CodexAppServerDriver {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn run_ephemeral_turn(
        &mut self,
        _run_id: &str,
        prompt: &str,
        timeout: Duration,
        redactor: &Secret,
        stream: &mut UnixStream,
    ) -> Result<i32, String> {
        let deadline = Instant::now() + timeout;
        let thread_start_id = self.send_rpc(
            "thread/start",
            json!({
                "cwd": self.workspace,
                "model": self.model,
                "modelProvider": "gewu",
                "approvalPolicy": "never",
                "sandbox": self.sandbox,
                "ephemeral": true,
            }),
        )?;
        let thread_res = self.wait_rpc(
            thread_start_id,
            Duration::from_secs(20),
            &mut |msg| {
                if let RpcSideMsg::Stderr(err) = msg {
                    let cleaned = redactor.redact(err);
                    if !cleaned.trim().is_empty() {
                        let _ = send_frame(stream, &RuntimeFrame::Stderr { line: cleaned });
                    }
                }
            },
        )?;
        let thread_id = thread_res
            .get("thread")
            .and_then(|t| t.get("id"))
            .or_else(|| thread_res.get("threadId"))
            .and_then(Value::as_str)
            .ok_or_else(|| "Codex thread/start returned no thread.id".to_owned())?
            .to_owned();

        let turn_start_id = self.send_rpc(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": prompt }],
            }),
        )?;

        let mut turn_done: Option<i32> = None;
        let mut client_broken = false;
        let handle = |msg: RpcSideMsg<'_>,
                      thread_id: &str,
                      redactor: &Secret,
                      stream: &mut UnixStream,
                      turn_done: &mut Option<i32>,
                      client_broken: &mut bool| match msg {
            RpcSideMsg::Notification(val) => {
                for mapped in translate_codex_v2_notification(val, thread_id) {
                    match mapped.get("type").and_then(Value::as_str) {
                        Some("turn.completed") => *turn_done = Some(0),
                        Some("turn.failed") => *turn_done = Some(1),
                        _ => {}
                    }
                    let cleaned = redactor.redact(&mapped.to_string());
                    if !send_frame(stream, &RuntimeFrame::Event { line: cleaned }) {
                        *client_broken = true;
                    }
                }
            }
            RpcSideMsg::Stderr(err) => {
                let cleaned = redactor.redact(err);
                if !cleaned.trim().is_empty()
                    && !send_frame(stream, &RuntimeFrame::Stderr { line: cleaned })
                {
                    *client_broken = true;
                }
            }
        };

        self.wait_rpc(turn_start_id, Duration::from_secs(20), &mut |msg| {
            handle(
                msg,
                &thread_id,
                redactor,
                stream,
                &mut turn_done,
                &mut client_broken,
            )
        })?;

        while turn_done.is_none() && !client_broken {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                let _ = self.send_rpc("thread/unsubscribe", json!({ "threadId": thread_id }));
                return Err("Codex app-server turn timed out".to_owned());
            }
            match self.rx.recv_timeout(rem.min(Duration::from_millis(200))) {
                Ok((true, line)) => {
                    if let Ok(val) = serde_json::from_str::<Value>(&line) {
                        handle(
                            RpcSideMsg::Notification(&val),
                            &thread_id,
                            redactor,
                            stream,
                            &mut turn_done,
                            &mut client_broken,
                        );
                    }
                }
                Ok((false, line)) => {
                    handle(
                        RpcSideMsg::Stderr(&line),
                        &thread_id,
                        redactor,
                        stream,
                        &mut turn_done,
                        &mut client_broken,
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err("Codex app-server exited mid-turn".to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Codex app-server stdout closed mid-turn".to_owned());
                }
            }
        }

        // Release ephemeral thread resources cleanly.
        let _ = self.send_rpc("thread/unsubscribe", json!({ "threadId": thread_id }));
        if client_broken {
            return Err("client disconnected during Codex turn".to_owned());
        }
        Ok(turn_done.unwrap_or(1))
    }

    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for CodexAppServerDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Translate one Codex `app-server` v2 notification into the canonical Codex JSON
/// event format already handled by `agent_run/native/outcome.rs`.
pub(crate) fn translate_codex_v2_notification(val: &Value, expected_thread: &str) -> Vec<Value> {
    let Some(method) = val.get("method").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(params) = val.get("params") else {
        return Vec::new();
    };
    if let Some(tid) = params.get("threadId").and_then(Value::as_str)
        && tid != expected_thread
    {
        return Vec::new();
    }
    match method {
        "item/started" | "item/completed" => {
            let Some(item) = params.get("item") else {
                return Vec::new();
            };
            let ev_type = if method == "item/started" {
                "item.started"
            } else {
                "item.completed"
            };
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
            let mapped = match kind {
                "commandExecution" => json!({
                    "type": "command_execution",
                    "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "command": item.get("command").and_then(Value::as_str).unwrap_or_default(),
                    "exit_code": item.get("exitCode"),
                    "aggregated_output": item.get("aggregatedOutput").and_then(Value::as_str).unwrap_or_default(),
                }),
                "mcpToolCall" => json!({
                    "type": "mcp_tool_call",
                    "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "tool": item.get("tool").and_then(Value::as_str).unwrap_or("forgejo"),
                    "arguments": item.get("arguments").cloned().unwrap_or(Value::Null),
                    "status": item.get("status").and_then(Value::as_str).unwrap_or_default(),
                    "result": item.get("result").cloned().unwrap_or(Value::Null),
                    "error": item.get("error").cloned().unwrap_or(Value::Null),
                }),
                "fileChange" => json!({
                    "type": "file_change",
                    "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "status": item.get("status").and_then(Value::as_str).unwrap_or_default(),
                }),
                "agentMessage" => {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| extract_content_text(item.get("content")));
                    json!({
                        "type": "agent_message",
                        "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "text": text,
                    })
                }
                _ => return Vec::new(),
            };
            vec![json!({ "type": ev_type, "item": mapped })]
        }
        "error" => {
            let msg = params
                .get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .and_then(Value::as_str)
                .unwrap_or("Codex app-server error");
            vec![json!({ "type": "error", "message": msg })]
        }
        "turn/completed" => {
            let turn = params.get("turn").unwrap_or(&Value::Null);
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            if status == "completed" {
                vec![json!({ "type": "turn.completed" })]
            } else {
                let err_msg = turn
                    .get("error")
                    .and_then(|e| e.get("message").or(Some(e)))
                    .and_then(Value::as_str)
                    .unwrap_or("the native turn failed");
                vec![json!({
                    "type": "turn.failed",
                    "error": { "message": err_msg }
                })]
            }
        }
        _ => Vec::new(),
    }
}

// ============================================================================
// 4. OpenCode (`opencode serve` + ephemeral `opencode run --server`) Driver
// ============================================================================

struct OpencodeServeDriver {
    server: Child,
    url: String,
    password: String,
    auth: String,
    config_dir: PathBuf,
    workspace: PathBuf,
    program: PathBuf,
    install_root: PathBuf,
    exe: PathBuf,
    rt_root: PathBuf,
    ro_dir: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    config: PathBuf,
    git_config: PathBuf,
    mcp_socket: Option<PathBuf>,
    external_env: BTreeMap<String, String>,
    external_secrets: BTreeMap<String, Secret>,
    model: String,
}

impl OpencodeServeDriver {
    fn spawn(host: &RoleRuntimeHost) -> Result<Self, String> {
        // Place OpenCode's generated config and skills strictly inside read-only `ro_dir`.
        let dir = host.ro_dir.join("opencode-cfg");
        let cfg = dir.join("opencode");
        crate::mode::ensure_dir_nofollow_under(&host.rt_root, &cfg)
            .map_err(|e| format!("{} cannot be created: {e}", cfg.display()))?;
        let npm = match host.protocol {
            Protocol::ChatCompletions => "@ai-sdk/openai-compatible",
            Protocol::Responses => "@ai-sdk/openai",
            Protocol::AnthropicMessages => "@ai-sdk/anthropic",
        };
        let total_ms = host.spec.timeout_secs.max(1).saturating_mul(1000);
        let req_ms = (total_ms.saturating_sub(total_ms / 5) / 12).clamp(1_000, 45_000);
        let mut mcp_map = serde_json::Map::new();
        if let Some(socket) = host.mcp_socket.as_ref() {
            let mut mcp_cmd = vec![
                host.exe.display().to_string(),
                "--mcp-forgejo-connect".to_owned(),
                socket.display().to_string(),
            ];
            if let Some(cookie) = host.mcp_cookie.as_ref() {
                mcp_cmd.push(cookie.display().to_string());
            }
            mcp_map.insert(
                "gewu_forgejo".to_owned(),
                json!({
                    "type": "local",
                    "command": mcp_cmd,
                    "enabled": true
                }),
            );
        }
        role_mcp::insert_opencode_servers(&mut mcp_map, &host.role_mcp_servers);
        let mut oc_cfg = json!({
            "$schema": "https://opencode.ai/config.json",
            "provider": {
                "gewu": {
                    "npm": npm,
                    "name": "gewu",
                    "options": {
                        "baseURL": host.spec.base_url,
                        "apiKey": format!("{{file:{}}}", host.key_file.display()),
                        "timeout": req_ms,
                        "maxRetries": 2
                    },
                    "models": {
                        host.spec.model.clone(): {
                            "name": host.spec.model.clone(),
                            "limit": {"context": 32000, "output": 4096}
                        }
                    }
                }
            },
            "model": format!("gewu/{}", host.spec.model),
        });
        if !mcp_map.is_empty() {
            oc_cfg["mcp"] = Value::Object(mcp_map);
        }
        write_json_under(&host.rt_root, &cfg.join("opencode.json"), &oc_cfg)?;
        for skill in &host.skills {
            if let Some(name) = skill.file_name() {
                copy_tree_under(&host.rt_root, skill, &cfg.join("skills").join(name))?;
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("OpenCode port cannot be allocated: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("OpenCode port cannot be read: {e}"))?
            .port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");
        let password = crate::mode::random_hex_256()
            .map_err(|e| format!("OpenCode password cannot be generated: {e}"))?;
        let auth = format!("Basic {}", base64_encode(format!("opencode:{password}").as_bytes()));

        let args = vec![
            "serve".to_owned(),
            "--hostname".to_owned(),
            "127.0.0.1".to_owned(),
            "--port".to_owned(),
            port.to_string(),
        ];
        let envs = vec![
            ("XDG_CONFIG_HOME".to_owned(), dir.display().to_string()),
            ("OPENCODE_PASSWORD".to_owned(), password.clone()),
        ];
        let mut cmd = host.base_confined_command(
            "opencode-serve.json",
            &host.spec.program,
            &args,
            &envs,
            &[],
        )?;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut server = cmd
            .spawn()
            .map_err(|e| format!("OpenCode resident server failed to start: {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ready = false;
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().expect("loopback"),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                ready = true;
                break;
            }
            if matches!(server.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            let _ = server.kill();
            let _ = server.wait();
            return Err("OpenCode resident server did not open its loopback port".to_owned());
        }
        std::thread::sleep(Duration::from_millis(600));

        // Register gewu_forgejo MCP once at resident server boot (eliminates per-wake cold boot).
        if let (Some(sock), Some(state_dir)) = (host.mcp_socket.as_ref(), host.mcp_state.as_ref()) {
            let http = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .new_agent();
            let _ = http
                .post(format!("{url}/api/session"))
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(
                    serde_json::to_vec(&json!({
                        "title": "warm",
                        "location": { "directory": host.spec.workspace }
                    }))
                    .expect("json"),
                );
            let mut mcp_cmd = vec![
                host.exe.display().to_string(),
                "--mcp-forgejo-connect".to_owned(),
                sock.display().to_string(),
            ];
            if let Some(cookie) = host.mcp_cookie.as_ref() {
                mcp_cmd.push(cookie.display().to_string());
            }
            let body = json!({"config": {
                "type": "local",
                "command": mcp_cmd,
                "codemode": true,
                "timeout": {"startup": 20000, "catalog": 20000, "execution": 120000}
            }});
            let _ = http
                .put(format!("{url}/api/experimental/mcp/gewu_forgejo"))
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(serde_json::to_vec(&body).expect("json"));
            let mcp_deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < mcp_deadline && !state_dir.join("tools-ready-6").is_file() {
                if matches!(server.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        Ok(Self {
            server,
            url,
            password,
            auth,
            config_dir: dir,
            workspace: host.workspace.clone(),
            program: host.spec.program.clone(),
            install_root: host.install_root.clone(),
            exe: host.exe.clone(),
            rt_root: host.rt_root.clone(),
            ro_dir: host.ro_dir.clone(),
            home: host.home.clone(),
            tmp: host.tmp.clone(),
            config: host.config.clone(),
            git_config: host.git_config.clone(),
            mcp_socket: host.mcp_socket.clone(),
            external_env: host.external_env.clone(),
            external_secrets: host.external_secrets.clone(),
            model: host.spec.model.clone(),
        })
    }
}

impl ResidentDriver for OpencodeServeDriver {
    fn is_alive(&mut self) -> bool {
        matches!(self.server.try_wait(), Ok(None))
    }

    fn run_ephemeral_turn(
        &mut self,
        run_id: &str,
        prompt: &str,
        timeout: Duration,
        redactor: &Secret,
        stream: &mut UnixStream,
    ) -> Result<i32, String> {
        let args = vec![
            "run".to_owned(),
            "--server".to_owned(),
            self.url.clone(),
            "--format".to_owned(),
            "json".to_owned(),
            "--auto".to_owned(),
            "--archived".to_owned(),
            "--title".to_owned(),
            format!("gewu-{run_id}"),
            "--model".to_owned(),
            format!("gewu/{}", self.model),
        ];
        let mut read_paths = vec![
            self.install_root.display().to_string(),
            self.ro_dir.display().to_string(),
        ];
        if let Some(exe_parent) = self.exe.parent() {
            read_paths.push(exe_parent.display().to_string());
        }
        if let Some(p) = self.mcp_socket.as_ref().and_then(|s| s.parent()) {
            read_paths.push(p.display().to_string());
        }
        let spec_file = self.ro_dir.join("opencode-run.json");
        write_json_under(
            &self.rt_root,
            &spec_file,
            &json!({
                "workspace": self.workspace.display().to_string(),
                "program": self.program.display().to_string(),
                "args": args,
                "read": read_paths,
                "write": [
                    self.home.display().to_string(),
                    self.tmp.display().to_string(),
                    self.config.display().to_string(),
                ],
            }),
        )?;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--confined-program")
            .arg(&spec_file)
            .current_dir(&self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env_clear();
        for (k, v) in crate::confinement::safe_environment(std::env::vars_os()) {
            cmd.env(k, v);
        }
        cmd.env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("XDG_CONFIG_HOME", &self.config_dir)
            .env("XDG_DATA_HOME", self.config.join("data"))
            .env("XDG_STATE_HOME", self.config.join("state"))
            .env("XDG_CACHE_HOME", self.config.join("cache"))
            .env("XDG_RUNTIME_DIR", self.config.join("run"))
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("OPENCODE_PASSWORD", &self.password);
        for (k, v) in &self.external_env {
            crate::engine::settings::validate_external_env_key(k)?;
            cmd.env(k, v);
        }
        for (k, s) in &self.external_secrets {
            crate::engine::settings::validate_external_env_key(k)?;
            cmd.env(k, s.expose());
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("OpenCode run client failed to start: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
        }
        let rx = spawn_stdio_readers(&mut child);
        let deadline = Instant::now() + timeout;
        let mut session_id: Option<String> = None;
        let exit_code = loop {
            while let Ok((is_stdout, line)) = rx.try_recv() {
                if is_stdout {
                    if session_id.is_none()
                        && let Ok(val) = serde_json::from_str::<Value>(&line)
                        && let Some(sid) = val
                            .get("sessionID")
                            .or_else(|| val.get("sessionId"))
                            .and_then(Value::as_str)
                    {
                        session_id = Some(sid.to_owned());
                    }
                    let cleaned = redactor.redact(&line);
                    if !send_frame(stream, &RuntimeFrame::Event { line: cleaned }) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("client disconnected during OpenCode run".to_owned());
                    }
                } else {
                    let cleaned = redactor.redact(&line);
                    if !cleaned.trim().is_empty() {
                        let _ = send_frame(stream, &RuntimeFrame::Stderr { line: cleaned });
                    }
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                while let Ok((is_stdout, line)) = rx.recv_timeout(Duration::from_millis(100)) {
                    let cleaned = redactor.redact(&line);
                    if is_stdout {
                        let _ = send_frame(stream, &RuntimeFrame::Event { line: cleaned });
                    } else if !cleaned.trim().is_empty() {
                        let _ = send_frame(stream, &RuntimeFrame::Stderr { line: cleaned });
                    }
                }
                break status.code().unwrap_or(1);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("OpenCode ephemeral run timed out".to_owned());
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // Best-effort cleanup of the ephemeral session on the resident OpenCode server.
        if let Some(sid) = session_id {
            let http = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(3)))
                .http_status_as_error(false)
                .build()
                .new_agent();
            let _ = http
                .delete(format!("{}/api/session/{sid}", self.url))
                .header("Authorization", &self.auth)
                .call();
        }
        Ok(exit_code)
    }

    fn shutdown(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Drop for OpencodeServeDriver {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

fn spawn_stdio_readers(child: &mut Child) -> mpsc::Receiver<(bool, String)> {
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    if let Some(stdout) = child.stdout.take() {
        let tx_out = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx_out.send((true, line)).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx_err = tx;
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx_err.send((false, line)).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

fn launch_budget(timeout_secs: u64) -> (u64, u64) {
    let total_ms = timeout_secs.max(1).saturating_mul(1000);
    let usable = total_ms.saturating_sub(total_ms / 5);
    let request_ms = (usable / 3).clamp(1_000, 120_000);
    let retry_delay_ms = (request_ms / 10).clamp(200, 4_000);
    (request_ms, retry_delay_ms)
}

fn write_codex_catalog(
    native_root: &Path,
    home: &Path,
    program: &Path,
    model: &str,
) -> Result<Option<PathBuf>, String> {
    let codex_home = native_root.join("codex");
    fs::create_dir_all(&codex_home)
        .map_err(|e| format!("Codex catalog home cannot be created: {e}"))?;
    let output = Command::new(program)
        .arg("debug")
        .arg("models")
        .env_clear()
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .map_err(|e| format!("Codex debug models failed: {e}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let catalog: Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let models = catalog
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let template = models
        .iter()
        .find(|e| e.get("slug").and_then(Value::as_str) == Some(model))
        .or_else(|| {
            models.iter().find(|e| {
                e.get("tool_mode").is_none_or(Value::is_null)
                    && e.get("use_responses_lite") != Some(&Value::Bool(true))
            })
        })
        .or_else(|| models.first());
    let Some(mut obj) = template.and_then(Value::as_object).cloned() else {
        return Ok(None);
    };
    for (k, v) in [
        ("slug", json!(model)),
        ("display_name", json!(model)),
        ("description", json!(model)),
        ("visibility", json!("list")),
        ("supported_in_api", json!(true)),
        ("priority", json!(1)),
        ("apply_patch_tool_type", json!("freeform")),
        ("shell_type", json!("unified_exec")),
        ("web_search_tool_type", json!("text")),
        ("supports_search_tool", json!(false)),
        ("use_responses_lite", json!(false)),
        ("tool_mode", Value::Null),
        ("multi_agent_version", Value::Null),
        ("supports_experimental_context", json!(false)),
    ] {
        obj.insert(k.to_owned(), v);
    }
    let path = codex_home.join("model-catalog.json");
    let bytes = serde_json::to_vec(&json!({ "models": [Value::Object(obj)] }))
        .map_err(|e| format!("Codex catalog JSON failed: {e}"))?;
    crate::mode::write_private_nofollow(&path, &bytes)
        .map_err(|e| format!("Codex catalog write failed: {e}"))?;
    Ok(Some(path))
}

fn write_json_under(root: &Path, path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("{} cannot be encoded: {e}", path.display()))?;
    crate::mode::write_private_nofollow_under(root, path, text.as_bytes())
        .map_err(|e| format!("{} cannot be written: {e}", path.display()))
}

fn copy_tree_under(root: &Path, src: &Path, dst: &Path) -> Result<(), String> {
    crate::mode::ensure_dir_nofollow_under(root, dst)
        .map_err(|e| format!("{} cannot be created: {e}", dst.display()))?;
    for entry in fs::read_dir(src)
        .map_err(|e| format!("{} cannot be read: {e}", src.display()))?
        .flatten()
    {
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if let Ok(meta) = fs::symlink_metadata(&p)
            && meta.file_type().is_symlink()
        {
            continue;
        }
        if p.is_dir() {
            copy_tree_under(root, &p, &target)?;
        } else if p.is_file() {
            let bytes = fs::read(&p).map_err(|e| format!("{} cannot be read: {e}", p.display()))?;
            crate::mode::write_private_nofollow_under(root, &target, &bytes)
                .map_err(|e| format!("{} cannot be written: {e}", target.display()))?;
        }
    }
    Ok(())
}

fn toml_esc(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_esc(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn base64_encode(input: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsh_sdk_events_translate_to_canonical_headless_shape() {
        let call = json!({
            "jsonrpc": "2.0",
            "method": "session.event",
            "params": {
                "sessionId": "run-1",
                "event": {
                    "type": "tool/call",
                    "data": { "callId": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}" }
                }
            }
        });
        let out = translate_dsh_sdk_notification(&call, "run-1");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "tool_call");
        assert_eq!(out[0]["callId"], "c1");
        assert_eq!(out[0]["tool"], "bash");
        assert_eq!(out[0]["input"]["command"], "ls");
        // Different sessionId is ignored (strict per-wake isolation).
        assert!(translate_dsh_sdk_notification(&call, "run-2").is_empty());

        let turn_end = json!({
            "jsonrpc": "2.0",
            "method": "session.event",
            "params": {
                "sessionId": "run-1",
                "event": {
                    "type": "turn/end",
                    "data": { "turn": 1, "reason": { "kind": "completed" } }
                }
            }
        });
        let end = translate_dsh_sdk_notification(&turn_end, "run-1");
        assert_eq!(end.len(), 1);
        assert_eq!(end[0]["type"], "status");
        assert_eq!(end[0]["phase"], "turn_end");
        assert_eq!(end[0]["reason"]["kind"], "completed");
    }

    #[test]
    fn codex_v2_events_translate_to_canonical_exec_shape() {
        let started = json!({
            "jsonrpc": "2.0",
            "method": "item/started",
            "params": {
                "threadId": "th-1",
                "item": { "type": "commandExecution", "id": "i1", "command": "git status" }
            }
        });
        let out = translate_codex_v2_notification(&started, "th-1");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "item.started");
        assert_eq!(out[0]["item"]["type"], "command_execution");
        assert_eq!(out[0]["item"]["command"], "git status");
        assert!(translate_codex_v2_notification(&started, "th-other").is_empty());

        let done = json!({
            "jsonrpc": "2.0",
            "method": "turn/completed",
            "params": { "threadId": "th-1", "turn": { "id": "t1", "status": "completed" } }
        });
        let out = translate_codex_v2_notification(&done, "th-1");
        assert_eq!(out[0]["type"], "turn.completed");
    }

    #[test]
    fn fingerprint_changes_when_permissions_or_role_env_change() {
        let base_perm = RolePermissions::default();
        let review_perm = RolePermissions {
            review: true,
            ..RolePermissions::default()
        };
        let empty_env = BTreeMap::new();
        let empty_sec = BTreeMap::new();
        let a = compute_fingerprint(
            "coder",
            Program::Dsh,
            "0.2.2-rc.5",
            Path::new("/bin/dsh"),
            Protocol::ChatCompletions,
            "http://127.0.0.1:1/v1",
            "m1",
            "",
            &base_perm,
            &empty_env,
            &empty_sec,
            Path::new("/nonexistent"),
            None,
            None,
        );
        let b = compute_fingerprint(
            "coder",
            Program::Dsh,
            "0.2.2-rc.5",
            Path::new("/bin/dsh"),
            Protocol::ChatCompletions,
            "http://127.0.0.1:1/v1",
            "m1",
            "",
            &review_perm,
            &empty_env,
            &empty_sec,
            Path::new("/nonexistent"),
            None,
            None,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn validate_run_paths_rejects_escape_and_symlink() {
        let root = crate::test_support::scratch_root("rt-validate-paths");
        let runs_root = root.join("agent-a");
        let other = root.join("agent-b/run-1/mcp-state");
        fs::create_dir_all(&runs_root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let outside_run = root.join("agent-b/run-1");
        assert!(
            validate_run_paths(
                &runs_root,
                &outside_run,
                &other.join("calls.jsonl"),
                &other.join("writes"),
            )
            .is_err(),
            "cross-role run_dir must be rejected"
        );
        // Symlinked run_dir under runs_root pointing outside must be rejected.
        let link_run = runs_root.join("run-link");
        std::os::unix::fs::symlink(&outside_run, &link_run).unwrap();
        assert!(
            validate_run_paths(
                &runs_root,
                &link_run,
                &link_run.join("mcp-state/calls.jsonl"),
                &link_run.join("mcp-state/writes"),
            )
            .is_err(),
            "symlinked run_dir must be rejected"
        );
        // Valid run_dir under runs_root succeeds.
        let good_run = runs_root.join("run-good");
        let good_mcp = good_run.join("mcp-state");
        fs::create_dir_all(&good_mcp).unwrap();
        assert!(
            validate_run_paths(
                &runs_root,
                &good_run,
                &good_mcp.join("calls.jsonl"),
                &good_mcp.join("writes"),
            )
            .is_ok()
        );
    }

    #[test]
    fn recover_stale_runtimes_sweeps_non_marker_descendant_of_dead_leader() {
        fn spawn_orphan_group(child_pid_file: &Path, reap_leader: bool) -> (Option<Child>, u32, u32) {
            let mut leader = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "sleep 60 </dev/null >/dev/null 2>&1 & echo $! > '{}'; exit 0",
                    child_pid_file.display()
                ))
                .process_group(0)
                .spawn()
                .expect("spawn leader");
            let pgid = leader.id();
            let leader_opt = if reap_leader {
                let _ = leader.wait();
                None
            } else {
                // Leave leader unreaped so it sits in zombie ('Z') state during recovery.
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && !process_is_dead_or_zombie(pgid) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Some(leader)
            };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && !child_pid_file.exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            let desc_pid: u32 = fs::read_to_string(child_pid_file)
                .expect("descendant pid")
                .trim()
                .parse()
                .expect("numeric pid");
            assert!(
                !process_is_dead_or_zombie(desc_pid),
                "non-marker descendant {desc_pid} should be alive before recovery"
            );
            (leader_opt, pgid, desc_pid)
        }

        fn assert_swept(desc_pid: u32, label: &str) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && !process_is_dead_or_zombie(desc_pid) {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                process_is_dead_or_zombie(desc_pid),
                "{label}: non-marker descendant {desc_pid} survived recover_stale_runtimes"
            );
        }

        // Case 1: reaped dead leader with `runtime.pid`.
        {
            let root = crate::test_support::scratch_root("rt-h8-reaped");
            let rt_dir = root.join("coder/runtime");
            fs::create_dir_all(&rt_dir).unwrap();
            let (_leader, pgid, desc_pid) =
                spawn_orphan_group(&root.join("desc1.pid"), true);
            fs::write(rt_dir.join(RUNTIME_PID_FILE), pgid.to_string()).unwrap();
            HarnessPool::recover_stale_runtimes(&root);
            assert_swept(desc_pid, "reaped leader");
        }

        // Case 2: unreaped zombie ('Z') leader with `runtime.pid`.
        {
            let root = crate::test_support::scratch_root("rt-h8-zombie");
            let rt_dir = root.join("coder/runtime");
            fs::create_dir_all(&rt_dir).unwrap();
            let (mut leader, pgid, desc_pid) =
                spawn_orphan_group(&root.join("desc2.pid"), false);
            assert!(
                Path::new(&format!("/proc/{pgid}")).exists(),
                "unreaped leader should still have a zombie /proc entry"
            );
            fs::write(rt_dir.join(RUNTIME_PID_FILE), pgid.to_string()).unwrap();
            HarnessPool::recover_stale_runtimes(&root);
            assert_swept(desc_pid, "zombie leader");
            if let Some(mut c) = leader.take() {
                let _ = c.wait();
            }
        }

        // Case 3: `runtime.pid` removed on clean self-exit, only `runtime.meta` tombstone remains.
        {
            let root = crate::test_support::scratch_root("rt-h8-meta-only");
            let rt_dir = root.join("coder/runtime");
            fs::create_dir_all(&rt_dir).unwrap();
            let (_leader, pgid, desc_pid) =
                spawn_orphan_group(&root.join("desc3.pid"), true);
            let meta = RuntimeMeta {
                pid: pgid,
                boot_id: current_boot_id().unwrap_or_default(),
                start_time: 0,
            };
            crate::mode::write_private_nofollow(
                rt_dir.join(RUNTIME_META_FILE),
                &serde_json::to_vec(&meta).unwrap(),
            )
            .unwrap();
            assert!(!rt_dir.join(RUNTIME_PID_FILE).exists());
            HarnessPool::recover_stale_runtimes(&root);
            assert_swept(desc_pid, "meta-only tombstone");
        }
    }
}
