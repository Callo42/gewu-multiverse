//! The file modes the two files on disk are held to.
//!
//! The credentials file holds secrets, so it is created as 0600 and checked back:
//! the program never assumes the mode took. The directory around it is 0700, so the
//! files are not reachable through a directory others can walk.

use std::fs;
use std::io;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Constant-time byte-slice equality check for authentication cookies and tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a 256-bit cryptographically random lowercase hex string from `/dev/urandom`.
pub fn random_hex_256() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(&mut bytes)?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

/// The only mode the credentials file may have: its user alone may read and write it.
pub const SECRET_FILE_MODE: u32 = 0o600;

/// The mode of the directory holding the files: its user alone may enter it.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Set the mode of an existing path (a file or a directory).
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// The mode of an existing path, with the file-type bits removed.
pub fn mode_of(path: &Path) -> io::Result<u32> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o7777)
}

/// Hold one existing directory, and every directory between it and `stop`, to mode
/// 0700. `stop` itself is included; nothing above it is ever touched, and a path
/// that is not below `stop` is held to 0700 on its own only.
///
/// `fs::create_dir_all` makes the intermediate directories with the process umask,
/// so a path like `<root>/agents/<name>` would otherwise leave `agents/` at `0775`
/// while `<name>` is `0700` (F-1). This walks the chain the product owns and gives
/// every link the one custody the product promises for its directories.
pub fn hold_dir_private_below(stop: &Path, path: &Path) -> io::Result<()> {
    if !path.starts_with(stop) {
        return set_mode(path, PRIVATE_DIR_MODE);
    }
    let mut current = Some(path);
    while let Some(dir) = current {
        set_mode(dir, PRIVATE_DIR_MODE)?;
        if dir == stop {
            break;
        }
        current = dir.parent();
    }
    Ok(())
}

/// Hold every path under one of the product's own roots to the one custody.
///
/// The product holds each directory it *creates* to `0700` and each file it *writes*
/// to `0600`, but a tree an older version left behind is never revisited, so an old
/// run directory or configuration file keeps the umask mode it was made with (R-1).
/// This walks a root and holds its directories to `0700` and its plain files to
/// `0600`.
///
/// A directory named `workspace` is held itself but **not** descended into: the clone
/// inside it is git's, not the product's, and its modes are not the product's to
/// change (the workspace holds no secret the custody rule protects; the `0700`
/// workspace is what blocks traversal). Symlinks are never followed, and a path that
/// cannot be changed is left as it is rather than failing the load.
pub fn hold_tree_private(root: &Path) {
    if !root.is_dir() {
        return;
    }
    let _ = set_mode(root, PRIVATE_DIR_MODE);
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let _ = set_mode(&path, PRIVATE_DIR_MODE);
            if path
                .file_name()
                .map(|name| name == "workspace")
                .unwrap_or(false)
            {
                continue;
            }
            hold_tree_private(&path);
        } else if file_type.is_file() {
            let _ = set_mode(&path, SECRET_FILE_MODE);
        }
    }
}

/// Render a mode the way `chmod` writes it, for people to read.
pub fn render(mode: u32) -> String {
    format!("{:04o}", mode)
}

/// Write a whole file with mode 0600, replacing whatever was there.
///
/// The mode is set at creation and checked afterwards, so a file that already
/// existed with a wider mode does not keep it. The staging file is created with
/// `O_CREAT | O_EXCL | O_NOFOLLOW` and a unique suffix so a symlink planted beside
/// the target can never hijack the write.
pub fn write_private(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    // A configuration file may be a symlink the person made on purpose. Write the
    // file it points at, so the link survives the atomic rename.
    let path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    write_private_atomic(&path, bytes)
}

/// Write a whole file with mode 0600, **strictly refusing symlinks** at the target
/// path or staging file.
///
/// Used for supervisor-owned runtime files, keys, cookies, pointers and harness
/// configs so an agent cannot plant a symlink to clobber arbitrary host files.
pub fn write_private_nofollow(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symlink and cannot be written", path.display()),
            ));
        }
        if !meta.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
    }
    write_private_atomic(path, bytes)
}

/// Read a private regular file with `O_NOFOLLOW`, verifying it is a regular file
/// owned by the current effective UID (never a symlink or FIFO).
pub fn read_private_regular(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let my_uid = rustix::process::geteuid().as_raw();
    if meta.uid() != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by the current user", path.display()),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// Open (creating missing directories with mode `0700`) `target` strictly beneath
/// `root` using fd-relative `mkdirat` + `openat(..., O_DIRECTORY | O_NOFOLLOW)`.
///
/// Every step from `root` down is pinned by directory file descriptor, so even a
/// concurrent `renameat2(RENAME_EXCHANGE)` swapping an intermediate directory with
/// a symlink cannot redirect traversal outside `root` (zero TOCTOU).
fn open_dir_nofollow_under(root: &Path, target: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::{Mode, fstat, mkdirat};
    use std::path::Component;

    let my_uid = rustix::process::geteuid().as_raw();
    let mut cur = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} must be a regular directory (not a symlink): {e}",
                root.display()
            ),
        )
    })?;
    let root_stat = fstat(&cur).map_err(|e| io::Error::other(e.to_string()))?;
    if root_stat.st_uid != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by the current user", root.display()),
        ));
    }
    let rel = target.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is outside protected root {}",
                target.display(),
                root.display()
            ),
        )
    })?;
    let dir_mode = Mode::from_bits_truncate(PRIVATE_DIR_MODE);
    for comp in rel.components() {
        match comp {
            Component::CurDir => continue,
            Component::Normal(seg) => {
                match mkdirat(&cur, seg, dir_mode) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(e) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "cannot create directory `{}` under {}: {e}",
                                seg.to_string_lossy(),
                                root.display()
                            ),
                        ));
                    }
                }
                let next = openat(
                    &cur,
                    seg,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "component `{}` under {} is a symlink or non-directory and cannot be traversed: {e}",
                            seg.to_string_lossy(),
                            root.display()
                        ),
                    )
                })?;
                let st = fstat(&next).map_err(|e| io::Error::other(e.to_string()))?;
                if st.st_uid != my_uid {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "component `{}` under {} is not owned by the current user",
                            seg.to_string_lossy(),
                            root.display()
                        ),
                    ));
                }
                cur = next;
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} contains disallowed path traversal", target.display()),
                ));
            }
        }
    }
    Ok(cur)
}

/// Create `target` strictly beneath `root` without following any symlink component.
pub fn ensure_dir_nofollow_under(root: &Path, target: &Path) -> io::Result<()> {
    let _fd = open_dir_nofollow_under(root, target)?;
    Ok(())
}

/// Write a mode-0600 file strictly beneath `root`, performing the entire directory
/// walk, staging file creation (`O_CREAT | O_EXCL | O_NOFOLLOW`), `fchmod`, and
/// `renameat` relative to the pinned parent directory file descriptor so concurrent
/// `renameat2(RENAME_EXCHANGE)` symlink swaps can never redirect the write outside `root`.
pub fn write_private_nofollow_under(
    root: &Path,
    path: impl AsRef<Path>,
    bytes: &[u8],
) -> io::Result<()> {
    use nix::errno::Errno;
    use nix::fcntl::{AtFlags, OFlag, openat, renameat};
    use nix::sys::stat::{Mode, SFlag, fstatat};
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let path = path.as_ref();
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let dir_fd = open_dir_nofollow_under(root, parent)?;

    match fstatat(&dir_fd, file_name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => {
            let kind = st.st_mode & SFlag::S_IFMT.bits();
            if kind == SFlag::S_IFLNK.bits() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is a symlink and cannot be written", path.display()),
                ));
            }
            if kind != SFlag::S_IFREG.bits() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a regular file", path.display()),
                ));
            }
            if (st.st_mode & 0o222) == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is read-only", path.display()),
                ));
            }
        }
        Err(Errno::ENOENT) => {}
        Err(e) => return Err(io::Error::other(e.to_string())),
    }

    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let rand_tag = random_hex_256()
        .map(|h| h[..16].to_owned())
        .unwrap_or_else(|_| format!("{seq}"));
    let name_str = file_name.to_str().unwrap_or("file");
    let stage_name = format!(".{name_str}.part-{}-{seq}-{rand_tag}", std::process::id());
    let stage_fd = openat(
        &dir_fd,
        stage_name.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(SECRET_FILE_MODE),
    )
    .map_err(|e| io::Error::other(format!("staging file under {} failed: {e}", parent.display())))?;

    let result = (|| -> io::Result<()> {
        let mut file = fs::File::from(stage_fd);
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(SECRET_FILE_MODE))?;
        file.sync_all()?;
        drop(file);
        renameat(&dir_fd, stage_name.as_str(), &dir_fd, file_name)
            .map_err(|e| io::Error::other(format!("atomic rename into {} failed: {e}", path.display())))
    })();
    if result.is_err() {
        let _ = unlinkat(&dir_fd, stage_name.as_str(), UnlinkatFlags::NoRemoveDir);
    }
    result
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // A rename into a writable directory can replace a read-only file, which the old
    // truncate-and-write could not do. Keep the refusal a read-only target earned:
    // the caller's one sentence says the file cannot be written.
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_symlink() && metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is read-only", path.display()),
            ));
        }
    }
    // Write a unique temporary file beside the target using O_CREAT | O_EXCL | O_NOFOLLOW,
    // set mode 0600 on the open file descriptor (`fchmod`, never path-based `chmod`),
    // and atomically rename it into place.
    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let rand_tag = random_hex_256()
        .map(|h| h[..16].to_owned())
        .unwrap_or_else(|_| format!("{seq}"));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let stage = path.with_file_name(format!(".{name}.part-{}-{seq}-{rand_tag}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(SECRET_FILE_MODE)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(&stage)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(SECRET_FILE_MODE))?;
        file.sync_all()?;
        fs::rename(&stage, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_rendered_with_leading_zeroes() {
        assert_eq!(render(0o600), "0600");
        assert_eq!(render(0o700), "0700");
    }

    #[test]
    fn a_tree_from_an_older_version_is_held_back_to_the_one_custody() {
        // R-1: a tree an older version wrote keeps its umask modes; the product holds
        // its own roots to the custody it promises on every load. A workspace's
        // contents are git's and are not touched.
        let root = crate::test_support::scratch_root("mode-hold-tree");
        let run = root.join("state").join("scribe").join("run-1");
        let workspace = root.join("agents").join("scribe").join("workspace");
        let clone = workspace.join("notes");
        fs::create_dir_all(&run).expect("the run directory");
        fs::create_dir_all(&clone).expect("the clone directory");
        let config = root.join("multiverse.toml");
        fs::write(&config, "x\n").expect("a file");
        let transcript = run.join("transcript.jsonl");
        fs::write(&transcript, "{}\n").expect("a file");
        let git_file = clone.join("HEAD");
        fs::write(&git_file, "ref\n").expect("a git file");
        for path in [&run, &workspace, &clone] {
            set_mode(path, 0o775).expect("widened for the test");
        }
        set_mode(&config, 0o664).expect("widened for the test");
        set_mode(&transcript, 0o664).expect("widened for the test");
        set_mode(&git_file, 0o644).expect("git's own mode");

        hold_tree_private(&root);

        for dir in [&run, &workspace] {
            assert_eq!(mode_of(dir).expect("mode"), PRIVATE_DIR_MODE, "{dir:?}");
        }
        assert_eq!(mode_of(&config).expect("mode"), SECRET_FILE_MODE);
        assert_eq!(mode_of(&transcript).expect("mode"), SECRET_FILE_MODE);
        assert_eq!(
            mode_of(&clone).expect("mode"),
            0o775,
            "a clone below the workspace is git's, not the product's"
        );
        assert_eq!(
            mode_of(&git_file).expect("mode"),
            0o644,
            "the clone's contents are git's, not the product's"
        );
    }

    #[test]
    fn a_private_write_always_leaves_mode_0600() {
        let root = crate::test_support::scratch_root("mode-private");
        let path = root.join("a-file");
        write_private(&path, b"first").expect("written");
        assert_eq!(mode_of(&path).expect("mode readable"), 0o600);
        set_mode(&path, 0o644).expect("widened for the test");
        write_private(&path, b"second").expect("written again");
        assert_eq!(mode_of(&path).expect("mode readable"), 0o600);
        assert_eq!(fs::read_to_string(&path).expect("readable"), "second");
    }

    #[test]
    fn write_private_nofollow_refuses_symlinks_and_preserves_target() {
        let root = crate::test_support::scratch_root("mode-nofollow");
        let target = root.join("victim.txt");
        fs::write(&target, "untouched").expect("victim");
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(write_private_nofollow(&link, b"clobbered").is_err());
        assert_eq!(fs::read_to_string(&target).expect("read"), "untouched");
        assert!(read_private_regular(&link).is_err());
    }

    #[test]
    fn write_private_nofollow_under_rejects_symlinked_ancestor_directory() {
        let root = crate::test_support::scratch_root("mode-ancestor-nofollow");
        let rt_root = root.join("rt");
        let victim = root.join("victim");
        fs::create_dir_all(&rt_root).expect("rt");
        fs::create_dir_all(&victim).expect("victim");
        fs::write(victim.join("config.toml"), "original").expect("victim config");
        std::os::unix::fs::symlink(&victim, rt_root.join("codex")).expect("symlink ancestor");

        let err = ensure_dir_nofollow_under(&rt_root, &rt_root.join("codex/sub"))
            .expect_err("symlinked ancestor dir must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        let err = write_private_nofollow_under(
            &rt_root,
            rt_root.join("codex/config.toml"),
            b"attacker",
        )
        .expect_err("write through symlinked ancestor must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(
            fs::read_to_string(victim.join("config.toml")).expect("read victim"),
            "original"
        );
    }

    #[test]
    fn write_private_nofollow_under_is_immune_to_concurrent_rename_exchange_race() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = crate::test_support::scratch_root("mode-n6-race");
        let rt_root = root.join("rt");
        let codex = rt_root.join("codex");
        let real_skills = codex.join("skills");
        let symlink_alt = codex.join("skills-link");
        let victim = root.join("victim");
        fs::create_dir_all(&real_skills).expect("skills");
        fs::create_dir_all(&victim).expect("victim");
        std::os::unix::fs::symlink(&victim, &symlink_alt).expect("symlink alt");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let codex_bg = codex.clone();
        let racer = std::thread::spawn(move || {
            use nix::fcntl::{OFlag, RenameFlags, open, renameat2};
            use nix::sys::stat::Mode;
            let dir_fd = open(
                &codex_bg,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .expect("open codex");
            while !stop_bg.load(Ordering::Relaxed) {
                let _ = renameat2(
                    &dir_fd,
                    "skills",
                    &dir_fd,
                    "skills-link",
                    RenameFlags::RENAME_EXCHANGE,
                );
            }
        });

        for i in 0..300 {
            let target = rt_root.join(format!("codex/skills/pwn-{i}.txt"));
            let _ = write_private_nofollow_under(&rt_root, &target, b"RACE-DATA");
            assert!(
                !victim.join(format!("pwn-{i}.txt")).exists(),
                "TOCTOU race escaped root on iteration {i}"
            );
        }
        stop.store(true, Ordering::Relaxed);
        let _ = racer.join();
        assert_eq!(
            fs::read_dir(&victim).expect("read victim").count(),
            0,
            "victim directory must remain completely empty"
        );
    }
}
