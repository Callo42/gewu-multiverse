//! The public one-line installer: argument errors, exclusive installation, atomic
//! binary replacement, stale/foreign locks and cancellation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[path = "../src/test_wait.rs"]
mod test_wait;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-install-sh-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("a scratch root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("the scratch mode");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_executable(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the executable directory");
    }
    fs::write(path, text).expect("the executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("the executable mode");
}

struct Release {
    scratch: Scratch,
    home: PathBuf,
    install: PathBuf,
    fakebin: PathBuf,
    fixture: PathBuf,
    tarball: String,
}

impl Release {
    fn new(label: &str, binary_body: &str) -> Self {
        let scratch = Scratch::new(label);
        let root = scratch.path();
        let home = root.join("home");
        let install = home.join(".local/bin");
        let fakebin = root.join("fakebin");
        let fixture = root.join("fixture");
        fs::create_dir_all(&home).expect("the home");
        fs::create_dir_all(&install).expect("the install directory");
        fs::create_dir_all(&fakebin).expect("the fake bin");
        fs::create_dir_all(&fixture).expect("the fixture directory");
        let tarball = "gewu-multiverse-abc-x86_64-linux.tar.gz".to_owned();
        let payload = root.join("payload/gewu-multiverse-abc-x86_64-linux");
        write_executable(&payload.join("gewu-multiverse"), binary_body);
        let status = Command::new("tar")
            .arg("-czf")
            .arg(fixture.join(&tarball))
            .arg("-C")
            .arg(root.join("payload"))
            .arg("gewu-multiverse-abc-x86_64-linux")
            .status()
            .expect("tar runs");
        assert!(status.success());
        let digest = sha256_file(&fixture.join(&tarball));
        let checksum = format!("{digest}  {tarball}\n");
        fs::write(fixture.join(format!("{tarball}.sha256")), checksum).expect("the checksum");
        write_executable(
            &fakebin.join("curl"),
            r#"#!/bin/sh
[ -n "${CURL_CALLED:-}" ] && : > "$CURL_CALLED"
out=
last=
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then out=$2; shift 2; continue; fi
  last=$1; shift
done
{
case "$last" in
  *releases/latest*) printf '%s\n' '{"tag_name":"v0.1.99"}' ;;
  *releases/tags/*) printf '%s\n' '{"assets":[{"name":"gewu-multiverse-abc-x86_64-linux.tar.gz"}]}' ;;
  *.tar.gz.sha256) cat "$FIXTURE/gewu-multiverse-abc-x86_64-linux.tar.gz.sha256" ;;
  *.tar.gz) cat "$FIXTURE/gewu-multiverse-abc-x86_64-linux.tar.gz" ;;
  *) exit 1 ;;
esac
} > "${out:-/dev/stdout}"
"#,
        );
        Self {
            scratch,
            home,
            install,
            fakebin,
            fixture,
            tarball,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("sh");
        command
            .arg(env!("CARGO_MANIFEST_DIR").to_string() + "/install.sh")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("INSTALL_DIR", &self.install)
            .env("FIXTURE", &self.fixture)
            .env("PATH", format!("{}:/usr/bin:/bin", self.fakebin.display()));
        command
    }

    fn old_binary(&self, body: &str) {
        write_executable(&self.install.join("gewu-multiverse"), body);
    }

    fn lock(&self) -> PathBuf {
        self.install.join(".gewu-install.lock")
    }
}

fn sha256_file(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum runs");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("a digest")
        .to_owned()
}

#[test]
fn argument_errors_are_clear_and_do_no_work() {
    let release = Release::new("arguments", "#!/bin/sh\nexit 0\n");
    for args in [
        vec!["--harness"],
        vec!["--bogus"],
        vec!["--harness", "potato"],
        vec!["--harness=unknown"],
    ] {
        let called = release.scratch.path().join("curl-called");
        let _ = fs::remove_file(&called);
        let output = release
            .command()
            .args(&args)
            .env("CURL_CALLED", &called)
            .output()
            .expect("install.sh runs");
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("install.sh:"), "{args:?}: {stderr}");
        assert!(!called.exists(), "{args:?} reached the network");
        assert!(!release.install.join("gewu-multiverse").exists());
    }
}

#[test]
fn a_bad_checksum_or_an_unrunnable_new_binary_keeps_the_installed_binary() {
    let release = Release::new("checksum", "#!/bin/sh\nexit 1\n");
    release.old_binary("#!/bin/sh\necho 'gewu-multiverse 0.1.30'\n");
    let old = fs::read(release.install.join("gewu-multiverse")).expect("the old binary");
    let checksum_path = release.fixture.join(format!("{}.sha256", release.tarball));
    fs::write(
        &checksum_path,
        format!("{}  {}\n", "0".repeat(64), release.tarball),
    )
    .expect("a wrong checksum");
    let output = release
        .command()
        .args(["--harness", "none"])
        .output()
        .expect("install.sh runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checksum verification failed"), "{stderr}");
    assert_eq!(
        fs::read(release.install.join("gewu-multiverse")).expect("the old binary"),
        old
    );
    assert!(!release.lock().exists());
    assert!(
        !fs::read_dir(&release.install)
            .expect("the install directory")
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".gewu-multiverse.new."))
    );

    // A correctly checksummed binary that does not run must not replace the old one.
    let digest = sha256_file(&release.fixture.join(&release.tarball));
    fs::write(&checksum_path, format!("{digest}  {}\n", release.tarball))
        .expect("the right checksum");
    let output = release
        .command()
        .args(["--harness", "none"])
        .output()
        .expect("install.sh runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("did not run here"), "{stderr}");
    assert_eq!(
        fs::read(release.install.join("gewu-multiverse")).expect("the old binary"),
        old
    );
    assert!(!release.lock().exists());
}

#[test]
fn the_lock_is_exclusive_and_a_foreign_lock_is_never_deleted() {
    let release = Release::new("lock", "#!/bin/sh\necho 'gewu-multiverse 0.1.99'\n");
    let lock = release.lock();
    fs::create_dir_all(&lock).expect("the foreign lock");
    fs::write(lock.join("pid"), format!("{}\n", std::process::id())).expect("the live pid");
    fs::write(lock.join("token"), "someone-elses-token\n").expect("the foreign token");
    let output = release
        .command()
        .args(["--harness", "none"])
        .output()
        .expect("install.sh runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(lock.join("pid").is_file());

    // A dead owner is reclaimed only when the directory is this installer's own.
    fs::write(lock.join("pid"), "999999\n").expect("the stale pid");
    let output = release
        .command()
        .args(["--harness", "none"])
        .output()
        .expect("install.sh runs");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unrecognised lock")
            || String::from_utf8_lossy(&output.stderr).contains("refusing"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(lock.is_dir(), "a foreign token was deleted");

    fs::write(lock.join("token"), "gewu-install-lock-v1\n").expect("the own token");
    let output = release
        .command()
        .args(["--harness", "none"])
        .output()
        .expect("install.sh runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!lock.exists(), "the stale own lock was not cleared");
    assert!(release.install.join("gewu-multiverse").is_file());
}

#[test]
fn cancelling_a_program_install_keeps_the_binary_and_removes_the_lock() {
    let release = Release::new(
        "cancel",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'gewu-multiverse 0.1.99'; exit 0; fi
if [ "$1" = "--install-forgejo" ]; then exit 0; fi
if [ "$1" = "--install-harness" ]; then : > "$HOME/harness-started"; exec sleep 30; fi
exit 1
"#,
    );
    let mut child = release
        .command()
        .args(["--harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("install.sh starts");
    let marker = release.home.join("harness-started");
    test_wait::wait_until("the program install to start", || marker.exists());
    let status = Command::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
    let status = child.wait().expect("install.sh ends");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert!(release.install.join("gewu-multiverse").is_file());
    assert!(!release.lock().exists());
    assert!(
        !fs::read_dir(&release.install)
            .expect("the install directory")
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".gewu-multiverse.new."))
    );
}

#[test]
fn the_new_binary_is_installed_with_its_version_after_a_valid_download() {
    let release = Release::new(
        "success",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'gewu-multiverse 0.1.99'; exit 0; fi
if [ "$1" = "--install-forgejo" ]; then exit 0; fi
if [ "$1" = "--install-harness" ]; then exit 0; fi
exit 1
"#,
    );
    let output = release
        .command()
        .args(["--harness", "pi,codex"])
        .output()
        .expect("install.sh runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let version = Command::new(release.install.join("gewu-multiverse"))
        .arg("--version")
        .output()
        .expect("the installed binary runs");
    assert!(String::from_utf8_lossy(&version.stdout).contains("0.1.99"));
    assert!(!release.lock().exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("the pi agent program is ready"), "{stderr}");
    assert!(
        stderr.contains("the codex agent program is ready"),
        "{stderr}"
    );
}
