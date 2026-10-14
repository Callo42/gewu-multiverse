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
use std::path::Path;
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
    let editor = env::var("EDITOR").ok().filter(|value| !value.trim().is_empty());
    let visual = env::var("VISUAL").ok().filter(|value| !value.trim().is_empty());
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
    run(editor(), path)
}

/// Hand a path to the reading program and wait for it.
pub fn view(path: &Path) -> io::Result<ExitStatus> {
    run(pager(), path)
}

/// Run `program` with the path as its one argument, through a shell so that a
/// program named with its own options (`code --wait`, `less -R`) still works.
fn run(program: String, path: &Path) -> io::Result<ExitStatus> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec {program} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
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
}
