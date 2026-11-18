//! Offline and bundled-Forgejo release archive installation (`install.sh --tarball`
//! and unpacked archives carrying `forgejo/forgejo-17.0.0-linux-amd64`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-bundled-archive-{label}-{}-{nanos}",
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
fn local_tarball_with_bundled_forgejo_installs_offline_without_curl() {
    let scratch = Scratch::new("offline-tarball");
    let root = scratch.path();
    let home = root.join("home");
    let install = home.join(".local/bin");
    let record = root.join("forgejo-source-seen.txt");
    fs::create_dir_all(&install).expect("install dir");

    let payload = root.join("payload/gewu-multiverse-deadbeef1234-x86_64-linux");
    let bin_script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'gewu-multiverse 0.1.99'; exit 0; fi\nif [ \"$1\" = --install-forgejo ]; then printf '%s' \"${{GEWU_FORGEJO_BINARY:-missing}}\" > '{}'; [ -f \"${{GEWU_FORGEJO_BINARY:-}}\" ]; exit $?; fi\nexit 1\n",
        record.display()
    );
    write_executable(&payload.join("gewu-multiverse"), &bin_script);
    write_executable(
        &payload.join("forgejo/forgejo-17.0.0-linux-amd64"),
        "#!/bin/sh\necho 'Forgejo 17.0.0'\n",
    );
    fs::write(payload.join("forgejo/LICENSE"), "GPLv3\n").expect("license");
    fs::write(payload.join("forgejo/FORGEJO-SOURCE.md"), "source\n").expect("source");

    let tarball_path = root.join("gewu-multiverse-deadbeef1234-x86_64-linux.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(root.join("payload"))
        .arg("gewu-multiverse-deadbeef1234-x86_64-linux")
        .status()
        .expect("tar runs");
    assert!(status.success());
    let digest = sha256_file(&tarball_path);
    fs::write(
        root.join("gewu-multiverse-deadbeef1234-x86_64-linux.tar.gz.sha256"),
        format!("{digest}  gewu-multiverse-deadbeef1234-x86_64-linux.tar.gz\n"),
    )
    .expect("sidecar");

    // Provide a PATH that deliberately has NO `curl` binary at all (only sh/tar/sha256sum/coreutils)
    // and place a poisoned `curl` in fakebin that fails if called.
    let fakebin = root.join("fakebin");
    let curl_called = root.join("curl-called");
    write_executable(
        &fakebin.join("curl"),
        &format!("#!/bin/sh\n: > '{}'\nexit 99\n", curl_called.display()),
    );

    let output = Command::new("sh")
        .arg(format!("{}/install.sh", env!("CARGO_MANIFEST_DIR")))
        .arg("--tarball")
        .arg(&tarball_path)
        .arg("--harness")
        .arg("none")
        .env_clear()
        .env("HOME", &home)
        .env("INSTALL_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .output()
        .expect("install.sh runs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "install.sh failed: {stderr}");
    assert!(
        !curl_called.exists(),
        "offline --tarball install invoked curl"
    );
    assert!(
        stderr.contains("installing bundled Forgejo 17.0.0"),
        "installer did not use bundled Forgejo: {stderr}"
    );
    let seen = fs::read_to_string(&record).expect("the binary saw GEWU_FORGEJO_BINARY");
    assert!(
        seen.ends_with("forgejo/forgejo-17.0.0-linux-amd64"),
        "unexpected GEWU_FORGEJO_BINARY: {seen}"
    );
    assert!(install.join("gewu-multiverse").is_file());
}

#[test]
fn local_tarball_refuses_mismatched_checksum_and_tampered_bundled_forgejo() {
    let scratch = Scratch::new("offline-tarball-refusal");
    let root = scratch.path();
    let home = root.join("home");
    let install = home.join(".local/bin");
    fs::create_dir_all(&install).expect("install dir");

    let payload = root.join("payload/gewu-multiverse-bad-x86_64-linux");
    // `--install-forgejo` fails (simulating SHA-256 mismatch on tampered bundled Forgejo).
    write_executable(
        &payload.join("gewu-multiverse"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'gewu-multiverse 0.1.99'; exit 0; fi\nif [ \"$1\" = --install-forgejo ]; then echo 'SHA-256 mismatch' >&2; exit 1; fi\nexit 1\n",
    );
    write_executable(
        &payload.join("forgejo/forgejo-17.0.0-linux-amd64"),
        "#!/bin/sh\nexit 0\n",
    );

    let tarball_path = root.join("gewu-multiverse-bad-x86_64-linux.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(root.join("payload"))
        .arg("gewu-multiverse-bad-x86_64-linux")
        .status()
        .expect("tar runs");
    assert!(status.success());

    // 1. Bad explicit --checksum is refused before replacing anything.
    let bad_sum = root.join("bad.sha256");
    fs::write(
        &bad_sum,
        format!(
            "{}  gewu-multiverse-bad-x86_64-linux.tar.gz\n",
            "0".repeat(64)
        ),
    )
    .expect("bad sum");
    let out = Command::new("sh")
        .arg(format!("{}/install.sh", env!("CARGO_MANIFEST_DIR")))
        .arg("--tarball")
        .arg(&tarball_path)
        .arg("--checksum")
        .arg(&bad_sum)
        .arg("--harness")
        .arg("none")
        .env("HOME", &home)
        .env("INSTALL_DIR", &install)
        .output()
        .expect("install.sh runs");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checksum verification failed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!install.join("gewu-multiverse").exists());

    // 2. Valid tarball checksum, but bundled Forgejo fails SHA-256 check -> exits non-zero!
    let digest = sha256_file(&tarball_path);
    let good_sum = root.join("good.sha256");
    fs::write(
        &good_sum,
        format!("{digest}  gewu-multiverse-bad-x86_64-linux.tar.gz\n"),
    )
    .expect("good sum");
    let out = Command::new("sh")
        .arg(format!("{}/install.sh", env!("CARGO_MANIFEST_DIR")))
        .arg("--tarball")
        .arg(&tarball_path)
        .arg("--checksum")
        .arg(&good_sum)
        .arg("--harness")
        .arg("none")
        .env("HOME", &home)
        .env("INSTALL_DIR", &install)
        .output()
        .expect("install.sh runs");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bundled Forgejo verification failed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
