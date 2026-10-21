//! The installer records the old version before it replaces the binary.
//!
//! A real v0.1.30 home has no `last-seen.toml`; the one-liner can still capture the
//! version the installed binary reports and write the marker, so the first open of
//! the new build shows `0.1.30 → 0.1.3x` instead of silently going to the board
//! (W31-1). This test runs the real `install.sh` with a loopback-free stand-in for
//! `curl`: no network is touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gewu-install-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a scratch root");
        Self(path)
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
    fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    fs::write(path, text).expect("the script");
    let mut mode = fs::metadata(path).expect("the script").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).expect("the mode");
}

fn run(command: &mut Command) -> std::process::Output {
    command.output().expect("the command runs")
}

#[test]
fn the_one_liner_records_the_version_it_replaces() {
    let scratch = Scratch::new("old-version");
    let root = scratch.path();
    let home = root.join("home");
    let install_dir = home.join(".local/bin");
    let config = home.join(".config/gewu-multiverse");
    fs::create_dir_all(&config).expect("the config root");
    fs::write(
        config.join("multiverse.toml"),
        "terms_acknowledged = true\n[endpoint]\nurl = \"http://127.0.0.1:9/v1/chat/completions\"\nmodel = \"demo\"\n[[agents]]\nname = \"agent-1\"\ndesired = \"stopped\"\n",
    )
    .expect("the old settings");
    write_executable(
        &install_dir.join("gewu-multiverse"),
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 'gewu-multiverse 0.1.30'\n",
    );

    // A tiny release: the new binary is a script with just the two answers the
    // installer asks for, and it prints one progress line on stderr.
    let payload = root.join("payload/gewu-multiverse-abc-x86_64-linux");
    write_executable(
        &payload.join("gewu-multiverse"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'gewu-multiverse 0.1.99'; exit 0; fi\nif [ \"$1\" = --install-forgejo ]; then echo 'fake fetch 50%' >&2; exit 0; fi\nexit 1\n",
    );
    let tarball = "gewu-multiverse-abc-x86_64-linux.tar.gz";
    fs::create_dir_all(root.join("fixture")).expect("the fixture directory");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(root.join("fixture").join(tarball))
        .arg("-C")
        .arg(root.join("payload"))
        .arg("gewu-multiverse-abc-x86_64-linux")
        .status()
        .expect("tar runs");
    assert!(status.success());
    let sum = run(Command::new("sha256sum").arg(root.join("fixture").join(tarball)));
    assert!(sum.status.success());
    let digest = String::from_utf8_lossy(&sum.stdout)
        .split_whitespace()
        .next()
        .expect("a digest")
        .to_owned();
    fs::write(
        root.join("fixture").join(format!("{tarball}.sha256")),
        format!("{digest}  {tarball}\n"),
    )
    .expect("the checksum file");

    let fakebin = root.join("fakebin");
    write_executable(
        &fakebin.join("curl"),
        r#"#!/bin/sh
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

    let output = run(Command::new("sh")
        .arg(env!("CARGO_MANIFEST_DIR").to_string() + "/install.sh")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("FIXTURE", root.join("fixture")));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "install.sh failed: {stderr}");
    let marker = fs::read_to_string(config.join("last-seen.toml")).expect("the marker");
    assert!(
        marker.contains("version = \"0.1.30\""),
        "the old version was not recorded: {marker}"
    );
    assert!(
        stderr.contains("progress is shown below") && stderr.contains("fake fetch 50%"),
        "the Forgejo fetch was silent: {stderr}"
    );
    let version = run(Command::new(install_dir.join("gewu-multiverse")).arg("--version"));
    assert!(String::from_utf8_lossy(&version.stdout).contains("0.1.99"));
}
