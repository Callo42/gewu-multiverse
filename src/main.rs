//! The entrypoint: open the terminal, draw the board, read keys, and put the
//! terminal back exactly as it was.
//!
//! The terminal is restored on every way out, including a panic: [`Screen`] puts it
//! back when it is dropped. A hand-off to `$EDITOR` or the pager leaves the board for
//! a moment and comes back to the same screen.
//!
//! The board is opened on the real engine ([`gewu_multiverse::engine::open_with`]).
//! `--demo` opens the same board on the scripted engine in
//! [`gewu_multiverse::board::fake`], and `--install-forgejo` fetches the pinned
//! Forgejo with one line of progress and exits.
//!
//! `--run-agent` and `--git-askpass` are not the board: they are the one-run process
//! and the credential answer the existing product uses. The engine starts its sessions
//! with the first, and the person's own `git` asks the second for an agent's token.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use gewu_multiverse::agent_run;
use gewu_multiverse::board::{Board, FakeEngine, HandoffKind};
use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::cli::{Cli, Invocation, help_text, parse_args};
use gewu_multiverse::editor;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::Desired;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::workspace;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use signal_hook::SigId;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

/// How long the loop waits for a key before drawing again.
const TICK: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // `git` runs `GIT_ASKPASS` as `<program> <prompt>`: the prompt is the program's
    // one argument, with no option of ours in between. The answer goes straight to
    // git over the pipe; nothing is printed to standard error.
    if std::env::var_os(workspace::ASKPASS_CREDENTIALS).is_some()
        && arguments.len() == 1
        && !arguments[0].starts_with('-')
    {
        return ExitCode::from(workspace::askpass_main(&arguments[0]).clamp(0, 255) as u8);
    }

    let invocation = match parse_args(arguments) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("gewu-multiverse: {message}");
            return ExitCode::FAILURE;
        }
    };
    match invocation {
        Invocation::Help => {
            println!("{}", help_text());
            ExitCode::SUCCESS
        }
        Invocation::Version => {
            println!("gewu-multiverse {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Invocation::RunAgent { run_dir } => {
            // One run of one agent, with the plan already written into run_dir. This
            // is the process the engine starts; it needs no terminal and no UI.
            let end = agent_run::execute(&run_dir);
            ExitCode::from(end.exit_code.clamp(0, 255) as u8)
        }
        Invocation::GitAskpass { prompt } => {
            ExitCode::from(workspace::askpass_main(&prompt).clamp(0, 255) as u8)
        }
        Invocation::InstallForgejo => match install_forgejo() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("gewu-multiverse: {error}");
                ExitCode::FAILURE
            }
        },
        Invocation::Run(cli) => {
            let outcome = if cli.demo {
                run_demo().map(|()| None)
            } else {
                run_engine(cli)
            };
            match outcome {
                Ok(quit_line) => {
                    if let Some(line) = quit_line {
                        println!("{line}");
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("gewu-multiverse: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Why the board could not start or could not put the terminal back.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    /// There is no terminal to draw on.
    #[error(
        "the board needs a terminal on standard input and standard output, so run it from a shell"
    )]
    NotATerminal,
    /// The terminal could not be put into, or taken out of, the board mode.
    #[error("the terminal could not be put into or taken out of the board mode: {0}")]
    Terminal(#[from] io::Error),
    /// The installation's directories or the engine could not be opened.
    #[error("{0}")]
    Engine(String),
}

/// Fetch the pinned Forgejo with one line of progress, for `--install-forgejo`.
///
/// The same installer the first start uses. Nothing is printed but the version, the
/// size and the percentage; a failure keeps nothing and names the next start.
fn install_forgejo() -> Result<(), StartupError> {
    let data_root = bundled_forgejo::data_root().map_err(StartupError::Engine)?;
    if bundled_forgejo::is_installed(&data_root) {
        println!(
            "gewu-multiverse: Forgejo {} is ready at {}",
            bundled_forgejo::VERSION,
            bundled_forgejo::binary_path(&data_root).display()
        );
        return Ok(());
    }
    println!(
        "gewu-multiverse: fetching Forgejo {} (about 122 MB) — the next start does this by itself when it is missing",
        bundled_forgejo::VERSION
    );
    let mut shown: Option<Option<u8>> = None;
    let installed = bundled_forgejo::install_forgejo_at(&data_root, |progress| {
        if shown == Some(progress.percent) {
            return;
        }
        shown = Some(progress.percent);
        let line = match progress.percent {
            Some(percent) => format!(
                "gewu-multiverse: fetching Forgejo {} — {percent}%",
                bundled_forgejo::VERSION
            ),
            None => format!(
                "gewu-multiverse: fetching Forgejo {} — {} MB",
                bundled_forgejo::VERSION,
                progress.received / (1024 * 1024)
            ),
        };
        if progress.percent.is_some() || progress.received > 0 {
            let _ = write!(io::stderr(), "\r{line}");
            let _ = io::stderr().flush();
        }
    });
    match installed {
        Ok(path) => {
            if shown.is_some() {
                eprintln!();
            }
            println!(
                "gewu-multiverse: Forgejo {} is ready at {}",
                bundled_forgejo::VERSION,
                path.display()
            );
            Ok(())
        }
        Err(error) => {
            if shown.is_some() {
                eprintln!();
            }
            Err(StartupError::Engine(error))
        }
    }
}

/// The layout the command line asked for, or the user's own.
fn layout_for(cli: &Cli) -> Result<Layout, StartupError> {
    let layout = match &cli.root {
        Some(root) => Layout::rooted_at(root.clone())
            .map_err(|error| StartupError::Engine(error.to_string()))?,
        None => {
            Layout::in_user_config_dir().map_err(|error| StartupError::Engine(error.to_string()))?
        }
    };
    Ok(match &cli.state_root {
        Some(state) => layout
            .with_state_root(state.clone())
            .map_err(|error| StartupError::Engine(error.to_string()))?,
        None => layout,
    })
}

/// Open the real engine and run the board until the person leaves or the terminal
/// closes. Returns the one line to print after the alternate screen is gone.
fn run_engine(cli: Cli) -> Result<Option<String>, StartupError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(StartupError::NotATerminal);
    }
    let layout = layout_for(&cli)?;
    let data_root = bundled_forgejo::data_root().map_err(StartupError::Engine)?;
    let options = EngineOptions::new(layout, data_root).map_err(StartupError::Engine)?;
    let engine = engine::open_with(options).map_err(StartupError::Engine)?;
    let mut board = Board::new(engine);
    let mut screen = Screen::enter()?;
    let termination = Termination::install()?;
    let drawn = event_loop(&mut screen, &mut board, Some(&termination));
    // The number of agents whose choice is started *now*, not when the board opened:
    // the person may have started one during this visit.
    let started = board
        .view()
        .agents
        .iter()
        .filter(|agent| agent.desired == Desired::Started)
        .count();
    // Stop every session and Forgejo before the terminal comes back.
    board.shutdown();
    let left = screen.leave();
    drawn.and(left)?;
    Ok((started > 0).then(|| {
        format!(
            "gewu-multiverse: stopped {started} agents and Forgejo — they start again when you reopen"
        )
    }))
}

/// Open the board with the scripted engine and run it until the person leaves.
fn run_demo() -> Result<(), StartupError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(StartupError::NotATerminal);
    }
    let mut board = Board::new(FakeEngine::new());
    let mut screen = Screen::enter()?;
    let drawn = event_loop(&mut screen, &mut board, None);
    // Stop every scripted session and Forgejo before the terminal comes back.
    board.shutdown();
    let left = screen.leave();
    drawn.and(left)?;
    Ok(())
}

/// Draw, then wait for a key, until the board says to leave or a terminal signal
/// (the window closing) arrives.
fn event_loop(
    screen: &mut Screen,
    board: &mut Board,
    termination: Option<&Termination>,
) -> io::Result<()> {
    loop {
        if termination.is_some_and(Termination::requested) {
            return Ok(());
        }
        board.tick();
        screen.terminal.draw(|frame| board.draw(frame))?;
        if board.should_quit() {
            return Ok(());
        }
        let polled = event::poll(TICK);
        match polled {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => board.on_key(key),
                Ok(Event::Paste(text)) => {
                    // A paste into the model form is ordinary text. The board's
                    // fields are edited one key at a time, so a paste is delivered
                    // as its characters in order.
                    for ch in text.chars().filter(|ch| !ch.is_control()) {
                        board.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            },
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        if let Some(handoff) = board.take_handoff() {
            let outcome = hand_to_the_user(screen, &handoff)?;
            board.handoff_finished();
            if outcome.interrupted {
                board.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }
            outcome.status?;
        }
    }
}

/// What came back from a hand-off: whether the board caught a `^C` while the
/// person's program owned the terminal, and how that program ended.
struct Handoff {
    interrupted: bool,
    status: io::Result<std::process::ExitStatus>,
}

/// Step out of the board, hand a path to `$EDITOR` or the pager, and step back in.
fn hand_to_the_user(
    screen: &mut Screen,
    handoff: &gewu_multiverse::board::Handoff,
) -> io::Result<Handoff> {
    screen.suspend()?;
    let guard = SigintGuard::install();
    let status = match handoff.kind {
        HandoffKind::Edit => {
            println!(
                "gewu-multiverse: opening {} in {}",
                handoff.label,
                editor::editor()
            );
            let _ = io::stdout().flush();
            editor::edit(&handoff.path)
        }
        HandoffKind::View => {
            println!(
                "gewu-multiverse: opening {} in the pager — q returns",
                handoff.label
            );
            let _ = io::stdout().flush();
            editor::view(&handoff.path)
        }
    };
    let interrupted = guard
        .as_ref()
        .map(|guard| guard.interrupted())
        .unwrap_or(false);
    drop(guard);
    screen.resume()?;
    Ok(Handoff {
        interrupted,
        status,
    })
}

/// Keep the board alive while a hand-off owns the terminal.
struct SigintGuard {
    flag: Arc<AtomicBool>,
    id: SigId,
}

impl SigintGuard {
    fn install() -> io::Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        let id = signal_hook::flag::register(SIGINT, Arc::clone(&flag))?;
        Ok(Self { flag, id })
    }

    fn interrupted(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.id);
    }
}

/// The terminal closing: `SIGHUP` when the terminal goes away, `SIGTERM` when the
/// window manager ends the program. Either one leaves the same clean shutdown path
/// as `q`.
struct Termination {
    flag: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl Termination {
    fn install() -> io::Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for signal in [SIGHUP, SIGTERM] {
            ids.push(signal_hook::flag::register(signal, Arc::clone(&flag))?);
        }
        Ok(Self { flag, ids })
    }

    fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl Drop for Termination {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// The terminal while the board is on it, or while it is stepped out of the way.
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
}

impl Screen {
    /// Put the terminal into the board mode: raw keys and the alternate screen.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(error) = crossterm::execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
        // Take the whole screen, not just the diff: a board started on a terminal
        // that still holds an earlier program's output must not let that show
        // through as if it were a value.
        terminal.clear()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    /// Step out of the board: hand the terminal back to the person's own program.
    fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        crossterm::execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableBracketedPaste
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Come back into the board after a program has finished with the terminal.
    fn resume(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        crossterm::execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableBracketedPaste
        )?;
        self.terminal.clear()
    }

    /// Put the terminal back for good. Safe to call more than once.
    fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        disable_raw_mode()?;
        crossterm::execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableBracketedPaste
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // A panic must not leave the terminal in raw mode.
        let _ = self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard the hand-off safety rests on: a `^C` that arrives while the
    /// person's program owns the terminal is caught by the board, not left to kill
    /// it and stop every session.
    #[test]
    fn a_ctrl_c_during_a_handoff_is_caught_instead_of_killing_the_board() {
        let guard = SigintGuard::install().expect("the signal handler is installed");
        assert!(!guard.interrupted());
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("kill -INT $PPID; exit 0")
            .status()
            .expect("the hand-off child runs");
        assert!(status.success());
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !guard.interrupted() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(guard.interrupted());
    }

    /// The terminal-signal guard records the two signals a closing terminal sends.
    #[test]
    fn a_terminal_signal_is_recorded() {
        let guard = Termination::install().expect("the signal handler is installed");
        assert!(!guard.requested());
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("kill -TERM $PPID; exit 0")
            .status()
            .expect("the child runs");
        assert!(status.success());
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !guard.requested() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(guard.requested());
    }
}
