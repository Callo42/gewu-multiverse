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
use std::os::fd::{AsFd, OwnedFd};
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
use gewu_multiverse::confinement;
use gewu_multiverse::editor;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::mcp_forgejo;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::workspace;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::pipe2;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use signal_hook::SigId;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

#[cfg(test)]
#[path = "test_wait.rs"]
mod test_wait;

/// How long the loop waits for a key before drawing again.
const TICK: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    // The shell record's diagnostic floor is the C library's own error vocabulary;
    // build it once at startup, not on the first failed tool call.
    agent_run::init_system_errors();

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
        Invocation::ConfinedShell => {
            ExitCode::from(confinement::run_confined_shell().clamp(0, 255) as u8)
        }
        Invocation::LandlockProbe { root } => {
            ExitCode::from(confinement::run_landlock_probe(&root).clamp(0, 255) as u8)
        }
        Invocation::ConfinedProgram { spec } => {
            ExitCode::from(confinement::run_confined_program(&spec).clamp(0, 255) as u8)
        }
        Invocation::McpForgejo { context } => {
            ExitCode::from(mcp_forgejo::serve(&context).clamp(0, 255) as u8)
        }
        Invocation::McpForgejoConnect { socket } => {
            ExitCode::from(mcp_forgejo::connect(&socket).clamp(0, 255) as u8)
        }
        Invocation::McpForgejoServe {
            socket,
            state,
            run,
            credentials,
            agent,
            workspace,
        } => ExitCode::from(
            mcp_forgejo::serve_supervised(&socket, &state, &run, &credentials, &agent, &workspace)
                .clamp(0, 255) as u8,
        ),
        Invocation::InstallHarness { program } => install_harness(&program),
        Invocation::PrintNativeKey { key } => match std::fs::read_to_string(&key) {
            Ok(text) => {
                println!("{}", text.trim());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("gewu-multiverse: the run's key cannot be read: {error}");
                ExitCode::FAILURE
            }
        },
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
                        // A lost terminal has no one to read this; do not panic on a
                        // dead standard output after the shutdown has finished.
                        let _ = writeln!(io::stdout(), "{line}");
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

/// Install one native program from the official package. Ctrl-C, SIGTERM and the
/// terminal closing stop at the next safe point and leave the previous version in
/// place; the CLI never leaves a half-written stage behind on a signal it can see.
fn install_harness(name: &str) -> ExitCode {
    let Some(program) = gewu_multiverse::harness::Program::parse(name) else {
        eprintln!("gewu-multiverse: there is no program named `{name}`");
        return ExitCode::FAILURE;
    };
    if !program.is_native() {
        eprintln!("gewu-multiverse: the built-in engine needs no install");
        return ExitCode::FAILURE;
    }
    let data_root = match bundled_forgejo::data_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("gewu-multiverse: {error}");
            return ExitCode::FAILURE;
        }
    };
    let cancel = gewu_multiverse::harness::CancelToken::new();
    let mut registered = Vec::new();
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        match signal_hook::flag::register(signal, cancel.flag()) {
            Ok(id) => registered.push(id),
            Err(error) => {
                eprintln!("gewu-multiverse: the installer cannot watch for signals: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    let mut last = String::new();
    let result =
        gewu_multiverse::harness::install_cancellable(&data_root, program, &cancel, |sample| {
            let text = match (sample.phase, sample.percent) {
                (gewu_multiverse::harness::InstallPhase::Downloading, Some(percent)) => {
                    format!("downloading {} — {percent}%", program.label())
                }
                (gewu_multiverse::harness::InstallPhase::Downloading, None) => {
                    format!("downloading {}…", program.label())
                }
                (gewu_multiverse::harness::InstallPhase::Unpacking, _) => {
                    format!("unpacking {}…", program.label())
                }
                (gewu_multiverse::harness::InstallPhase::InstallingPackages, _) => {
                    format!("installing {}'s locked packages…", program.label())
                }
                (gewu_multiverse::harness::InstallPhase::Verifying, _) => {
                    format!("checking {}…", program.label())
                }
                (gewu_multiverse::harness::InstallPhase::Preparing, _) => {
                    format!("preparing the {} install…", program.label())
                }
            };
            if text != last {
                last = text.clone();
                eprintln!("gewu-multiverse: {text}");
            }
        });
    for id in registered {
        signal_hook::low_level::unregister(id);
    }
    match result {
        Ok(installed) => {
            println!(
                "gewu-multiverse: {} {} is ready at {}",
                program.label(),
                installed.version,
                installed.executable.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) if error == gewu_multiverse::harness::INSTALL_CANCELED => {
            eprintln!("gewu-multiverse: install canceled; the previous version is unchanged");
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!(
                "gewu-multiverse: {} could not be installed: {error}",
                program.label()
            );
            ExitCode::FAILURE
        }
    }
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
    // The signal handlers are installed before the slow open: a terminal lost during
    // recovery records the request, and the loop ends and stops everything at once.
    let termination = Termination::install()?;
    let layout = layout_for(&cli)?;
    let data_root = bundled_forgejo::data_root().map_err(StartupError::Engine)?;
    let options = EngineOptions::new(layout, data_root).map_err(StartupError::Engine)?;
    // Open the terminal before the slow recovery, so a killed run's cleanup is
    // visible as a sentence instead of a blank alternate screen. Only a live earlier
    // process earns the line; an ordinary open goes straight to the board.
    let pending = engine::stale_recovery_claim(&options.layout, &options.data_root);
    let mut screen = Screen::enter()?;
    if let Some(claim) = pending {
        screen.message(&claim.message())?;
    }
    let engine = engine::open_with(options).map_err(StartupError::Engine)?;
    let mut board = Board::new(engine);
    let drawn = event_loop(&mut screen, &mut board, Some(&termination));
    // The number of agents whose choice is started *now*, not when the board opened:
    // the person may have started one during this visit.
    let started = board.quit_stopped_count();
    // Stop every session and Forgejo before the terminal comes back.
    board.shutdown();
    // A lost terminal has nothing to restore; asking for the mode back on a dead
    // pseudo-terminal would only fail after the shutdown has already done the work.
    let lost = matches!(drawn, Ok(LoopEnd::Lost));
    if lost {
        screen.abandon();
    }
    let left = if lost { Ok(()) } else { screen.leave() };
    drawn.and(left)?;
    Ok((started > 0).then(|| stopped_line(started)))
}

/// The line printed after stopping started agents, with the noun in agreement (U7).
fn stopped_line(started: usize) -> String {
    format!(
        "gewu-multiverse: stopped {} {} and Forgejo — they start again when you reopen",
        started,
        if started == 1 { "agent" } else { "agents" }
    )
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
    let lost = matches!(drawn, Ok(LoopEnd::Lost));
    if lost {
        screen.abandon();
    }
    let left = if lost { Ok(()) } else { screen.leave() };
    drawn.and(left)?;
    Ok(())
}

/// How a run of the board ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopEnd {
    /// The person quit, or a signal asked the program to stop.
    Finished,
    /// The terminal went away: end-of-file, `EIO`, `SIGHUP`, or a closed session.
    Lost,
}

/// How one wait for the terminal ended.
enum InputRead {
    Event(Event),
    Idle,
    Closed,
}

/// The terminal input the board loop reads. The tests replace it with a scripted
/// source that ends or fails on demand.
trait TerminalInput {
    fn read(&mut self, timeout: Duration) -> io::Result<InputRead>;
}

struct CrosstermInput;

impl TerminalInput for CrosstermInput {
    fn read(&mut self, timeout: Duration) -> io::Result<InputRead> {
        match event::poll(timeout) {
            Ok(true) => event::read().map(InputRead::Event),
            Ok(false) => Ok(InputRead::Idle),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(InputRead::Idle),
            Err(error) if terminal_is_gone(&error) => Ok(InputRead::Closed),
            Err(error) => Err(error),
        }
    }
}

/// Whether an I/O error means this terminal has ended rather than a passing problem.
fn terminal_is_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    ) || matches!(error.raw_os_error(), Some(5 | 6 | 25))
}

/// What the loop does with the terminal around the board.
trait BoardUi {
    fn draw(&mut self, board: &mut Board) -> io::Result<()>;
    /// Send a control sequence to the terminal itself (the copy route, F-13).
    fn copy(&mut self, _sequence: &str) -> io::Result<()> {
        Ok(())
    }
    fn handoff(
        &mut self,
        board: &mut Board,
        handoff: &gewu_multiverse::board::Handoff,
        stop: &dyn Fn() -> bool,
    ) -> io::Result<()>;
}

struct ScreenUi<'a> {
    screen: &'a mut Screen,
}

impl BoardUi for ScreenUi<'_> {
    fn draw(&mut self, board: &mut Board) -> io::Result<()> {
        self.screen.terminal.draw(|frame| board.draw(frame))?;
        Ok(())
    }

    fn copy(&mut self, sequence: &str) -> io::Result<()> {
        use std::io::Write;
        self.screen
            .terminal
            .backend_mut()
            .write_all(sequence.as_bytes())?;
        self.screen.terminal.backend_mut().flush()
    }

    fn handoff(
        &mut self,
        board: &mut Board,
        handoff: &gewu_multiverse::board::Handoff,
        stop: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        let outcome = hand_to_the_user(self.screen, handoff, stop)?;
        board.handoff_finished();
        if outcome.interrupted {
            board.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        }
        if let Some(status) = outcome.status {
            status?;
        }
        Ok(())
    }
}

/// Draw and read keys until the board says to leave, a signal asks the program to
/// stop, or the terminal ends. `shutdown` runs exactly once on every way out.
fn run_board(
    board: &mut Board,
    stop: &dyn Fn() -> bool,
    input: &mut dyn TerminalInput,
    ui: &mut dyn BoardUi,
) -> io::Result<LoopEnd> {
    let outcome = (|| -> io::Result<LoopEnd> {
        loop {
            if stop() {
                return Ok(LoopEnd::Finished);
            }
            board.tick();
            if ui.draw(board).is_err() {
                // A dead standard output is a lost terminal, not a reason to keep
                // drawing frames nobody can see.
                return Ok(LoopEnd::Lost);
            }
            if board.should_quit() {
                return Ok(LoopEnd::Finished);
            }
            match input.read(TICK) {
                Ok(InputRead::Event(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    board.on_key(key);
                    if let Some(sequence) = board.take_clipboard() {
                        ui.copy(&sequence)?;
                    }
                }
                Ok(InputRead::Event(Event::Paste(text))) => {
                    // A paste into the model form is ordinary text. The board's
                    // fields are edited one key at a time, so a paste is delivered
                    // as its characters in order.
                    for ch in text.chars().filter(|ch| !ch.is_control()) {
                        board.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
                    }
                }
                Ok(InputRead::Event(_)) | Ok(InputRead::Idle) => {}
                // End-of-file and every input error end the board: a loop that
                // retried a dead terminal is what used to spin at full CPU.
                Ok(InputRead::Closed) | Err(_) => return Ok(LoopEnd::Lost),
            }
            if let Some(handoff) = board.take_handoff() {
                ui.handoff(board, &handoff, stop)?;
            }
        }
    })();
    board.shutdown();
    outcome
}

/// Draw, then wait for a key, until the board says to leave or a terminal signal
/// (the window closing) arrives.
fn event_loop(
    screen: &mut Screen,
    board: &mut Board,
    termination: Option<&Termination>,
) -> io::Result<LoopEnd> {
    // A pseudo-terminal whose other end closed turns reads into `EIO`; crossterm's
    // Unix reader retries that error forever. The watcher notices the hang-up and
    // points standard input at an idle non-blocking pipe, so the reader returns, the
    // loop sees the loss, and `shutdown` runs once instead of a spin.
    let hangup = Hangup::install();
    let stop = || {
        termination.is_some_and(Termination::requested) || hangup.as_ref().is_some_and(Hangup::lost)
    };
    let mut input = CrosstermInput;
    let mut ui = ScreenUi { screen };
    let result = run_board(board, &stop, &mut input, &mut ui);
    if hangup.as_ref().is_some_and(Hangup::lost) {
        return Ok(LoopEnd::Lost);
    }
    result
}

/// A watcher for the terminal's own hang-up, and the idle pipe its input is moved to.
struct Hangup {
    lost: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    // Kept open: standard input is moved to `read`, and `write` keeps it idle
    // instead of at end-of-file.
    _read: OwnedFd,
    _write: OwnedFd,
}

impl Hangup {
    fn install() -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        let (read, write) = pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).ok()?;
        let lost = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_read = read.try_clone().ok()?;
        let thread_lost = Arc::clone(&lost);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let stdin = io::stdin();
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::empty())];
                match poll(&mut fds, PollTimeout::from(50u16)) {
                    Ok(0) => continue,
                    Ok(_) => {
                        let hung_up = fds[0].revents().is_some_and(|events| {
                            events.intersects(
                                PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL,
                            )
                        });
                        if !hung_up {
                            continue;
                        }
                        thread_lost.store(true, Ordering::SeqCst);
                        let _ = nix::unistd::dup2_stdin(&thread_read);
                        return;
                    }
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => return,
                }
            }
        });
        Some(Self {
            lost,
            stop,
            thread: Some(thread),
            _read: read,
            _write: write,
        })
    }

    fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for Hangup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What came back from a hand-off: whether the board caught a `^C` while the
/// person's program owned the terminal, and how that program ended. `status` is
/// `None` when the stop predicate won before a program was started.
struct Handoff {
    interrupted: bool,
    status: Option<io::Result<std::process::ExitStatus>>,
}

/// Step out of the board, hand a path to `$EDITOR` or the pager, and step back in.
///
/// `stop` is the same terminating-signal/lost-terminal predicate the board loop
/// uses. The editor wait polls it; when it turns true the program is ended and reaped
/// and this returns at once, so `run_board` reaches its one `shutdown`.
fn hand_to_the_user(
    screen: &mut Screen,
    handoff: &gewu_multiverse::board::Handoff,
    stop: &dyn Fn() -> bool,
) -> io::Result<Handoff> {
    if stop() {
        return Ok(Handoff {
            interrupted: false,
            status: None,
        });
    }
    if let Err(error) = screen.suspend() {
        if stop() {
            screen.abandon();
            return Ok(Handoff {
                interrupted: false,
                status: None,
            });
        }
        return Err(error);
    }
    let guard = SigintGuard::install();
    let status = match handoff.kind {
        HandoffKind::Edit => {
            println!(
                "gewu-multiverse: opening {} in {}",
                handoff.label,
                editor::editor()
            );
            let _ = io::stdout().flush();
            editor::edit_with_stop(&handoff.path, stop)
        }
        HandoffKind::View => {
            println!(
                "gewu-multiverse: opening {} in the pager — q returns",
                handoff.label
            );
            let _ = io::stdout().flush();
            editor::view_with_stop(&handoff.path, stop)
        }
    };
    let interrupted = guard
        .as_ref()
        .map(|guard| guard.interrupted())
        .unwrap_or(false);
    drop(guard);
    // A lost terminal has nothing to resume; a caught signal may or may not have
    // taken the terminal with it. Try only when the terminal is still usable.
    if stop() {
        if screen.resume().is_err() {
            screen.abandon();
        }
    } else {
        screen.resume()?;
    }
    Ok(Handoff {
        interrupted,
        status: Some(status),
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

    /// One sentence on the otherwise blank first screen, before the engine opens.
    /// It is replaced by the board's first frame; nothing else is drawn.
    fn message(&mut self, text: &str) -> io::Result<()> {
        use ratatui::layout::Alignment;
        use ratatui::text::Line;
        use ratatui::widgets::Paragraph;
        self.terminal.draw(|frame| {
            let area = frame.area();
            let line = Line::from(text.to_owned()).alignment(Alignment::Center);
            frame.render_widget(Paragraph::new(line), area);
        })?;
        Ok(())
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

    /// The terminal is gone: there is nothing left to restore, and asking for the
    /// cursor or the mode back would only fail on a dead pseudo-terminal.
    fn abandon(&mut self) {
        self.active = false;
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
    use crate::test_wait::{recv_within, wait_until};

    #[test]
    fn the_final_line_uses_the_right_noun() {
        // U7: `stopped 1 agents` was the plural for one.
        assert_eq!(
            stopped_line(1),
            "gewu-multiverse: stopped 1 agent and Forgejo — they start again when you reopen"
        );
        assert_eq!(
            stopped_line(2),
            "gewu-multiverse: stopped 2 agents and Forgejo — they start again when you reopen"
        );
    }

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
        wait_until("the caught interrupt", || guard.interrupted());
        assert!(guard.interrupted());
    }

    /// A scripted terminal input: one outcome per read, then idle.
    enum FakeRead {
        Closed,
        Error(io::Error),
        Key(KeyCode),
    }

    struct ScriptedInput {
        reads: Vec<FakeRead>,
    }

    impl TerminalInput for ScriptedInput {
        fn read(&mut self, _timeout: Duration) -> io::Result<InputRead> {
            match self.reads.pop() {
                Some(FakeRead::Closed) => Ok(InputRead::Closed),
                Some(FakeRead::Error(error)) => Err(error),
                Some(FakeRead::Key(code)) => Ok(InputRead::Event(Event::Key(KeyEvent::from(code)))),
                None => Ok(InputRead::Idle),
            }
        }
    }

    struct NoUi;

    impl BoardUi for NoUi {
        fn draw(&mut self, _board: &mut Board) -> io::Result<()> {
            Ok(())
        }

        fn handoff(
            &mut self,
            _board: &mut Board,
            _handoff: &gewu_multiverse::board::Handoff,
            _stop: &dyn Fn() -> bool,
        ) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn end_of_file_ends_the_board_and_shuts_down_once() {
        let mut board = Board::new(FakeEngine::new());
        let mut input = ScriptedInput {
            reads: vec![FakeRead::Closed],
        };
        let end = run_board(&mut board, &|| false, &mut input, &mut NoUi)
            .expect("a lost terminal is a clean end");
        assert_eq!(end, LoopEnd::Lost);
        assert_eq!(
            board.shutdown_calls(),
            1,
            "shutdown ran {board:?}",
            board = "board"
        );
    }

    #[test]
    fn an_input_error_ends_the_board_and_shuts_down_once() {
        let mut board = Board::new(FakeEngine::new());
        let mut input = ScriptedInput {
            reads: vec![FakeRead::Error(io::Error::from_raw_os_error(5))],
        };
        let end = run_board(&mut board, &|| false, &mut input, &mut NoUi)
            .expect("EIO is a lost terminal, not a crash");
        assert_eq!(end, LoopEnd::Lost);
        assert_eq!(board.shutdown_calls(), 1);
    }

    #[test]
    fn a_terminal_signal_flag_is_honoured_without_waiting_for_a_key() {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&flag);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            setter.store(true, Ordering::SeqCst);
        });
        // The board must end from the flag alone, with no key to wake it. A loop
        // that waited for a key would never return; the liveness receive fails the
        // test instead of hanging it.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            let mut board = Board::new(FakeEngine::new());
            let mut input = ScriptedInput { reads: Vec::new() };
            let end = run_board(
                &mut board,
                &|| flag.load(Ordering::SeqCst),
                &mut input,
                &mut NoUi,
            )
            .expect("the loop ends");
            let _ = done_tx.send((end, board.shutdown_calls()));
        });
        let (end, shutdown_calls) =
            recv_within("the board to honour the terminal-signal flag", &done_rx);
        runner.join().expect("the board loop ends");
        handle.join().expect("the setter ends");
        assert_eq!(end, LoopEnd::Finished);
        assert_eq!(shutdown_calls, 1);
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
        wait_until("the closing terminal's signal", || guard.requested());
        assert!(guard.requested());
    }

    /// A stop requested while the board is in a hand-off ends the person's program,
    /// reaps it, returns to the loop, and shuts the engine down exactly once.
    #[test]
    fn a_stop_during_a_hand_off_ends_the_child_and_shuts_down_once() {
        use gewu_multiverse::engine_api::{Command, EngineApi};

        let mut engine = FakeEngine::new();
        engine.send(Command::AcknowledgeTerms).expect("terms");
        engine
            .send(Command::SetEndpoint {
                url: "https://model.example/v1/chat/completions".to_owned(),
                model: "demo-model".to_owned(),
                key: Some("demo-key".to_owned()),
            })
            .expect("endpoint");
        engine
            .send(Command::CreateAgents { count: 1 })
            .expect("one agent");
        engine.send(Command::StartForgejo).expect("Forgejo");
        for _ in 0..122 {
            engine.tick();
        }
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        engine.tick();
        let _ = engine.board();

        let mut board = Board::new(engine);
        board.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            board.screen(),
            gewu_multiverse::board::Screen::Agent,
            "the board did not reach the agent screen"
        );

        let root = std::env::temp_dir().join(format!(
            "gewu-main-handoff-stop-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("the test root");
        let pid_file = root.join("editor.pid");
        let script = root.join("fake-editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\ntrap '' TERM HUP\nprintf '%s' \"$$\" > '{}'\nwhile :; do read -t 1 _ || true; done\n",
                pid_file.display()
            ),
        )
        .expect("the fake editor is written");
        let mut permissions = std::fs::metadata(&script)
            .expect("the fake editor metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&script, permissions).expect("the fake editor is executable");

        struct HandoffUi {
            stop: Arc<AtomicBool>,
            script: std::path::PathBuf,
            path: std::path::PathBuf,
            status: Option<io::Result<std::process::ExitStatus>>,
        }

        impl BoardUi for HandoffUi {
            fn draw(&mut self, _board: &mut Board) -> io::Result<()> {
                Ok(())
            }

            fn handoff(
                &mut self,
                _board: &mut Board,
                _handoff: &gewu_multiverse::board::Handoff,
                _stop: &dyn Fn() -> bool,
            ) -> io::Result<()> {
                let stop = Arc::clone(&self.stop);
                self.status = Some(editor::handoff_with(
                    self.script.to_str().expect("a UTF-8 path"),
                    &self.path,
                    false,
                    &|| stop.load(Ordering::SeqCst),
                ));
                Ok(())
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut ui = HandoffUi {
            stop: Arc::clone(&stop),
            script: script.clone(),
            path: root.join("instruction.md"),
            status: None,
        };
        let setter = {
            let stop = Arc::clone(&stop);
            let pid_file = pid_file.clone();
            let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                wait_until("the fake editor's pid record", || pid_file.is_file());
                stop.store(true, Ordering::SeqCst);
                let _ = stopped_tx.send(std::time::Instant::now());
            });
            (handle, stopped_rx)
        };
        let mut input = ScriptedInput {
            reads: vec![FakeRead::Key(KeyCode::Char('e'))],
        };
        let end = run_board(
            &mut board,
            &|| stop.load(Ordering::SeqCst),
            &mut input,
            &mut ui,
        )
        .expect("the hand-off ends cleanly");
        let _stopped_at = recv_within("the hand-off stop request", &setter.1);
        setter.0.join().expect("the setter ends");

        assert_eq!(end, LoopEnd::Finished);
        assert_eq!(
            board.shutdown_calls(),
            1,
            "shutdown did not run exactly once"
        );
        let status = ui
            .status
            .expect("the hand-off ran")
            .expect("the hand-off returns");
        assert!(!status.success(), "the stopped editor exited successfully");
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the editor wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the hand-off child {pid} was left behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
