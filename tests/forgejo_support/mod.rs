//! A throwaway Forgejo for the integration test.
//!
//! The instance is the Forgejo release named by `GEWU_FORGEJO_BIN`, run as a plain
//! process with a temporary root under the system temporary directory: an `app.ini`
//! of its own, an SQLite database of its own, repositories of its own and a port of
//! its own. It listens on `127.0.0.1` only, nothing else on the machine is touched,
//! and it is torn down — process and directory — when the test ends, successfully or
//! not.
//!
//! When `GEWU_FORGEJO_BIN` is not set the test says so and does nothing: the
//! repository does not carry a 120 MB server binary, so an environment without one
//! skips the integration test rather than pretending it passed.

#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A fresh scratch directory for one test.
pub fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("gewu-forgejo-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A running Forgejo, owned by one test.
pub struct TestForgejo {
    /// The temporary root: configuration, database, repositories and log.
    pub dir: PathBuf,
    /// The binary that was started.
    pub bin: PathBuf,
    /// The port it listens on.
    pub port: u16,
    /// Its base URL, without a trailing slash.
    pub url: String,
    child: Option<Child>,
}

impl TestForgejo {
    /// Start a Forgejo, or `None` when no binary was named.
    pub fn start(tag: &str) -> Option<Self> {
        let bin = std::env::var("GEWU_FORGEJO_BIN").ok()?;
        let bin = PathBuf::from(bin);
        if !bin.is_file() {
            eprintln!(
                "GEWU_FORGEJO_BIN is set to {} but that is not a file; skipping",
                bin.display()
            );
            return None;
        }
        let dir = scratch(tag);
        std::fs::create_dir_all(dir.join("data").join("repositories")).expect("data directory");
        let port = free_port();
        let url = format!("http://127.0.0.1:{port}");
        let config = dir.join("app.ini");
        std::fs::write(&config, app_ini(&dir, port)).expect("app.ini is written");
        let log = std::fs::File::create(dir.join("server.log")).expect("the log is created");
        let child = Command::new(&bin)
            .arg("web")
            .arg("--config")
            .arg(&config)
            .env("FORGEJO_WORK_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("the log clones")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("the Forgejo process is started");
        let server = Self {
            dir,
            bin,
            port,
            url,
            child: Some(child),
        };
        server.wait_until_ready();
        Some(server)
    }

    /// Wait for the instance to answer its health check.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if let Ok(response) = ureq::get(format!("{}/api/healthz", self.url)).call() {
                if response.status().is_success() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!(
            "the Forgejo at {} did not become ready; see {}",
            self.url,
            self.dir.join("server.log").display()
        );
    }

    /// Run the server's own command-line tool against this instance.
    pub fn cli(&self, args: &[&str]) -> Output {
        let output = Command::new(&self.bin)
            .args(args)
            .arg("--config")
            .arg(self.dir.join("app.ini"))
            .env("FORGEJO_WORK_DIR", &self.dir)
            .output()
            .expect("the Forgejo command-line tool runs");
        if !output.status.success() {
            panic!(
                "forgejo {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output
    }

    /// Create an account, through the server's own command-line tool.
    pub fn create_user(&self, login: &str, password: &str, admin: bool) {
        let email = format!("{login}@example.org");
        let mut args = vec![
            "admin",
            "user",
            "create",
            "--username",
            login,
            "--password",
            password,
            "--email",
            email.as_str(),
            "--must-change-password=false",
        ];
        if admin {
            args.push("--admin");
        }
        let output = self.cli(&args);
        if !String::from_utf8_lossy(&output.stdout).contains("successfully created") {
            panic!(
                "the account {login} was not created: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    /// Make an access token for an account with the server's own tool, and return it.
    ///
    /// The tool's default rights are used, which is enough for the instance's own
    /// administrator to be an administrator.
    pub fn access_token_for(&self, login: &str) -> String {
        let output = self.cli(&[
            "admin",
            "user",
            "generate-access-token",
            "--username",
            login,
            "--token-name",
            "gewu-test-token",
            "--raw",
        ]);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// One raw API call, for the few things the tool itself does not do (creating a
    /// repository in a test, for one account or for another).
    pub fn api(&self, token: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let url = format!("{}{}", self.url, path);
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .new_agent();
        let authorization = format!("token {token}");
        let response = match method {
            "GET" => agent
                .get(&url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .call(),
            "POST" | "PUT" | "PATCH" => {
                let mut request = match method {
                    "POST" => agent.post(&url),
                    "PUT" => agent.put(&url),
                    _ => agent.patch(&url),
                }
                .header("Accept", "application/json")
                .header("Authorization", &authorization);
                if let Some(body) = body {
                    request = request.header("Content-Type", "application/json");
                    request.send(body)
                } else {
                    request.send_empty()
                }
            }
            other => panic!("the test does not use {other}"),
        }
        .expect("the request is sent");
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().unwrap_or_default();
        (status, text)
    }

    /// Create a repository owned by the token's own account.
    pub fn create_repo(&self, token: &str, name: &str, private: bool) {
        let body = serde_json::json!({
            "name": name,
            "private": private,
            "auto_init": true,
            "default_branch": "main",
        })
        .to_string();
        let (status, text) = self.api(token, "POST", "/api/v1/user/repos", Some(&body));
        assert!(
            (200..300).contains(&status),
            "creating {name} answered {status}: {text}"
        );
    }

    /// Create a repository owned by another account, as the instance's administrator.
    pub fn create_repo_for(&self, admin_token: &str, login: &str, name: &str, private: bool) {
        let body = serde_json::json!({
            "name": name,
            "private": private,
            "auto_init": true,
            "default_branch": "main",
        })
        .to_string();
        let path = format!("/api/v1/admin/users/{login}/repos");
        let (status, text) = self.api(admin_token, "POST", &path, Some(&body));
        assert!(
            (200..300).contains(&status),
            "creating {login}/{name} answered {status}: {text}"
        );
    }
}

impl Drop for TestForgejo {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The temporary root goes with the instance; nothing outside it was touched.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A port that is free right now.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port can be reserved");
    listener.local_addr().expect("a local address").port()
}

/// A minimal `app.ini`: its own root, an SQLite database, no SSH, no registration.
///
/// The two keys a fresh instance needs are generated by Forgejo itself on first
/// start, so this file carries no secret and no key of its own.
fn app_ini(dir: &Path, port: u16) -> String {
    let dir = dir.display();
    format!(
        "APP_NAME = gewu-multiverse test instance\n\
         RUN_USER = {user}\n\
         WORK_PATH = {dir}\n\
         RUN_MODE = prod\n\
         \n\
         [database]\n\
         DB_TYPE = sqlite3\n\
         PATH = {dir}/data/forgejo.db\n\
         \n\
         [server]\n\
         PROTOCOL = http\n\
         DOMAIN = 127.0.0.1\n\
         HTTP_PORT = {port}\n\
         ROOT_URL = http://127.0.0.1:{port}/\n\
         DISABLE_SSH = true\n\
         OFFLINE_MODE = true\n\
         START_SSH_SERVER = false\n\
         \n\
         [security]\n\
         INSTALL_LOCK = true\n\
         \n\
         [service]\n\
         DISABLE_REGISTRATION = true\n\
         \n\
         [repository]\n\
         ROOT = {dir}/data/repositories\n\
         \n\
         [log]\n\
         MODE = console\n\
         LEVEL = warn\n",
        user = whoami(),
    )
}

/// The login to run the instance as. Forgejo refuses to run as a user other than the
/// one it is configured for, so the instance is configured for whoever runs the test.
fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_owned())
}
