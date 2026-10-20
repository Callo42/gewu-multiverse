//! Editing and reading files through the user's own programs.
//!
//! The terminal UI does not grow its own editor. When the user asks to edit an
//! agent's instruction or skills, or to read a run's log, the tool steps out of the
//! way and hands the file to the program the user already chose: `$EDITOR` (or
//! `$VISUAL`) for editing, `$PAGER` for reading. The terminal is put back exactly as
//! it was when the program exits.
//!
//! The path is always passed as an argument, never pasted into the command text, so a
//! name with a space or a quote in it cannot turn into something else. No secret is
//! ever handed to either program, because neither is ever given the credentials file.

use std::env;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// Which editing program to use, given the two variables that can name one.
///
/// `$EDITOR` wins over `$VISUAL`; with neither, `vi` is the honest last resort,
/// because it is the editor a Unix system is expected to have.
pub fn choose_editor(editor: Option<String>, visual: Option<String>) -> String {
    editor
        .filter(|value| !value.trim().is_empty())
        .or_else(|| visual.filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_owned())
}

/// Which reading program to use, given `$PAGER`.
pub fn choose_pager(pager: Option<String>) -> String {
    pager
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "less -R".to_owned())
}

/// The editing program this process would use.
///
/// `$EDITOR` and `$VISUAL` win when either is set. With neither, `nano` is chosen
/// ahead of `vi` when it is installed: `vi` is the honest last resort, but a person
/// who set no editor is exactly the person who does not know it, so a friendlier
/// editor is preferred when the machine has one.
pub fn editor() -> String {
    let editor = env::var("EDITOR")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let visual = env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if editor.is_some() || visual.is_some() {
        return choose_editor(editor, visual);
    }
    if program_on_path("nano") {
        "nano".to_owned()
    } else {
        "vi".to_owned()
    }
}

/// Whether an executable named `name` is on `PATH`.
fn program_on_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

/// Whether the user has chosen an editor through `$EDITOR` or `$VISUAL`.
///
/// When neither is set the tool falls back to `nano` or `vi`; that fallback is
/// exactly the case where the user needs to be told which program is about to take
/// the screen, so the UI asks first.
pub fn editor_is_configured() -> bool {
    env::var("EDITOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .is_some()
        || env::var("VISUAL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some()
}

/// The reading program this process would use.
pub fn pager() -> String {
    choose_pager(env::var("PAGER").ok())
}

/// Hand a path to the editing program and wait for it.
pub fn edit(path: &Path) -> io::Result<ExitStatus> {
    run(editor(), path, Handoff::Edit)
}

/// Hand a path to the reading program and wait for it.
pub fn view(path: &Path) -> io::Result<ExitStatus> {
    run(pager(), path, Handoff::View)
}

/// Which kind of hand-off is running, so its own scratch files can be suppressed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handoff {
    Edit,
    View,
}

/// The program name a hand-off string names: its first word, without a directory.
///
/// `$EDITOR` may carry options (`nano -w`, `code --wait`), so the name is the first
/// whitespace-separated word, and it may be an absolute path.
fn program_name(program: &str) -> &str {
    let first = program.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

/// A directory the product owns for the lifetime of one hand-off.
///
/// A program told to keep its state under `$XDG_DATA_HOME` writes there instead of
/// in the person's `HOME`; the directory is removed when the hand-off is over, so
/// nothing is left behind anywhere (F7). It is created with the product's one
/// custody, `0700`, and a hand-off that is `SIGKILL`ed cannot run its own cleanup, so
/// every hand-off first sweeps the stale roots a dead process left (O2).
struct Scratch {
    dir: PathBuf,
}

/// The prefix every hand-off scratch root carries under the system temporary
/// directory, and the mark that separates the owning process's id from the instant.
const SCRATCH_PREFIX: &str = "gewu-multiverse-handoff-";

impl Scratch {
    fn make() -> io::Result<Self> {
        sweep_stale_scratch();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("{SCRATCH_PREFIX}{}-{nanos}", std::process::id()));
        // `create_dir_all` makes the directory with the process umask (`0775` here),
        // not the `0700` every other directory the product makes is held to. The
        // scratch holds a program's own state, so it gets the same custody (O2).
        std::fs::DirBuilder::new()
            .mode(crate::mode::PRIVATE_DIR_MODE)
            .create(&dir)?;
        Ok(Self { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Remove scratch roots a hand-off that was killed could not remove itself.
///
/// The name carries the owning process's id, so a root whose process is no longer
/// alive is a dead hand-off's and is removed. A root whose process is alive belongs
/// to a hand-off still running (this one or a sibling UI) and is left alone. A pid
/// the operating system has since reused makes the sweep skip a dead root until that
/// process ends; the next hand-off then removes it. Errors are ignored: the sweep is
/// custody, not a screen.
pub fn sweep_stale_scratch() {
    let root = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(SCRATCH_PREFIX) else {
            continue;
        };
        let Some((pid, _instant)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if !crate::run::process_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The program string and environment a hand-off gets, so the program's own state
/// does not land in the person's `HOME` (F7).
///
/// Every program a hand-off spawns is enumerated and given a state location of its
/// own where the product can name one: `less` is told its history file is
/// `/dev/null`; `nano` is given an `$XDG_DATA_HOME` in a scratch directory (its
/// history and position files go under `nano/` there, not `~/.local/share/nano`);
/// `vi`, `vim`, `nvim` and `ex` are given `-i NONE`, so no viminfo file is written.
/// A program the person chose with `$EDITOR`/`$VISUAL`/`$PAGER` that this product
/// does not know keeps its own files: it is named in the hand-off sentence on the
/// primary screen, and the product does not pretend to know what it writes.
fn handoff_plan(
    program: &str,
    handoff: Handoff,
    scratch: &Path,
) -> (String, Vec<(String, String)>) {
    let name = program_name(program);
    let mut program = program.to_owned();
    let mut env: Vec<(String, String)> = Vec::new();
    if handoff == Handoff::View {
        // `less` writes `~/.lesshst` unless it is told not to (H-6).
        env.push(("LESSHISTFILE".to_owned(), "/dev/null".to_owned()));
    }
    if matches!(
        name,
        "vi" | "vim" | "nvim" | "ex" | "vim.tiny" | "vim.basic"
    ) {
        // vim writes `~/.viminfo` on exit; `-i NONE` suppresses it.
        program.push_str(" -i NONE");
    }
    if name == "nano" {
        // nano writes `$XDG_DATA_HOME/nano` (default `~/.local/share/nano`).
        env.push(("XDG_DATA_HOME".to_owned(), scratch.display().to_string()));
    }
    (program, env)
}

/// Run `program` with the path as its one argument, through a shell so that a
/// program named with its own options (`code --wait`, `less -R`) still works.
fn run(program: String, path: &Path, handoff: Handoff) -> io::Result<ExitStatus> {
    let scratch = Scratch::make()?;
    let (program, env) = handoff_plan(&program, handoff, &scratch.dir);
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("exec {program} \"$1\""))
        .arg("sh")
        .arg(path);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command.status();
    drop(scratch);
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_wins_over_visual() {
        assert_eq!(
            choose_editor(Some("nano".to_owned()), Some("emacs".to_owned())),
            "nano"
        );
    }

    #[test]
    fn visual_is_used_when_editor_is_absent_or_blank() {
        assert_eq!(choose_editor(None, Some("emacs".to_owned())), "emacs");
        assert_eq!(
            choose_editor(Some("   ".to_owned()), Some("emacs".to_owned())),
            "emacs"
        );
    }

    #[test]
    fn vi_is_the_last_resort_when_no_editor_is_installed() {
        // The pure chooser still falls back to `vi`; `editor()` may prefer `nano`
        // when the machine has one, which is tested below.
        assert_eq!(choose_editor(None, None), "vi");
    }

    #[test]
    fn an_explicit_editor_is_never_overridden_by_nano() {
        // Setting `$EDITOR` to `vi` is a choice; the fallback must not second-guess it.
        // The environment is process-wide, so only the pure chooser is asserted here.
        assert_eq!(choose_editor(Some("vi".to_owned()), None), "vi");
        assert_eq!(choose_editor(Some("emacs".to_owned()), None), "emacs");
    }

    #[test]
    fn a_pager_hand_off_writes_no_history_into_the_users_home() {
        // H-6: `less` wrote `~/.lesshst` when `o`/`T` handed it the log or transcript.
        let scratch = Path::new("/tmp/fxr12-scratch");
        let (_, env) = handoff_plan("less -R", Handoff::View, scratch);
        let env: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(env["LESSHISTFILE"], "/dev/null");
    }

    #[test]
    fn every_program_the_product_chooses_is_given_its_own_state_location() {
        // F7: `~/.lesshst` was gone after H-6 but `~/.local/share/nano` remained,
        // because `-N` is `--noconvert`, not "no history". Each program the product
        // itself picks now has a state location of its own.
        let scratch = Path::new("/tmp/fxr12-scratch");
        let (nano, env) = handoff_plan("nano", Handoff::Edit, scratch);
        assert_eq!(nano, "nano");
        let env: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(env["XDG_DATA_HOME"], "/tmp/fxr12-scratch");
        for vi in ["vi", "vim", "nvim", "/usr/bin/vim"] {
            let (program, _) = handoff_plan(vi, Handoff::Edit, scratch);
            assert!(program.ends_with(" -i NONE"), "{program}");
        }
        let (less, env) = handoff_plan("less", Handoff::View, scratch);
        let env: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(env["LESSHISTFILE"], "/dev/null");
        assert!(!env.contains_key("XDG_DATA_HOME"));
        assert_eq!(less, "less");
    }

    #[test]
    fn a_pager_is_chosen_with_a_sensible_last_resort() {
        assert_eq!(choose_pager(Some("more".to_owned())), "more");
        assert_eq!(choose_pager(None), "less -R");
        assert_eq!(choose_pager(Some(String::new())), "less -R");
    }

    #[test]
    fn a_path_with_a_space_is_passed_as_one_argument() {
        // The shell is told to pass `$1` through, and the path is given as that
        // argument, so nothing in the path is interpreted as shell text.
        let dir = crate::test_support::scratch_root("editor-space");
        let path = dir.join("a file with spaces.md");
        std::fs::write(&path, "content\n").expect("the file is written");
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg("exec cat \"$1\" >/dev/null")
            .arg("sh")
            .arg(&path)
            .status()
            .expect("the shell runs");
        assert!(status.success());
    }

    #[test]
    fn the_hand_off_scratch_root_is_private_and_a_dead_one_is_swept() {
        // O2: the scratch root the product makes under the system temporary directory
        // was created with the process umask (`0775` here), not the `0700` every other
        // directory the product makes is held to, and a hand-off killed with SIGKILL
        // could not run its `Drop`, so it was left behind.
        let scratch = Scratch::make().expect("a scratch root");
        assert_eq!(
            crate::mode::mode_of(&scratch.dir).expect("the scratch mode"),
            crate::mode::PRIVATE_DIR_MODE,
            "the scratch root does not carry the product's custody"
        );

        // A hand-off whose process is gone: the next hand-off removes it.
        let mut child = Command::new("true").spawn().expect("a throwaway child");
        let dead_pid = child.id();
        let _ = child.wait();
        let stale = std::env::temp_dir().join(format!("{SCRATCH_PREFIX}{dead_pid}-1"));
        std::fs::create_dir_all(&stale).expect("a stale scratch root");
        crate::mode::set_mode(&stale, 0o775).expect("the stale mode is set");
        sweep_stale_scratch();
        assert!(
            !stale.exists(),
            "a hand-off that cannot run its own cleanup left its root behind"
        );

        // A root whose process is alive is not swept.
        let live = Scratch::make().expect("a live scratch root");
        sweep_stale_scratch();
        assert!(live.dir.is_dir(), "a live hand-off's root was swept");
    }
}
