//! The entrypoint: open the terminal, draw the screens, read keys, and put the
//! terminal back exactly as it was.
//!
//! The terminal is restored on every way out, including a panic: [`Screen`] puts it
//! back when it is dropped. Running without a terminal is refused with a sentence,
//! not a panic.
//!
//! One branch of the program does not draw anything: `--run-agent <run dir>` is one
//! agent run, and it is how the tool starts an agent — as a child process of itself.
//! It reads its plan, does the work, writes its log and transcript, and exits.
//! Another, `--git-askpass <prompt>`, is how the user's own `git` asks this program
//! for an agent's token: the answer goes straight to git over a pipe, so the token is
//! never an argument, a URL, a log line or a screen.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use gewu_multiverse::agent_run;
use gewu_multiverse::app::{ActionKind, App, PendingAction};
use gewu_multiverse::cli::{Cli, HELP, Invocation, parse_args};
use gewu_multiverse::editor;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::ui;
use gewu_multiverse::workspace;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// How long the loop waits for a key before drawing again.
const TICK: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // `git` runs `GIT_ASKPASS` as `<program> <prompt>`: the prompt is the program's
    // one argument, with no option of ours in between. When the tool set the askpass
    // environment for a git command, that one argument is the prompt, and this is the
    // branch that answers it. The answer is written to standard output, which is the
    // pipe git reads; nothing goes to standard error.
    if std::env::var_os(gewu_multiverse::workspace::ASKPASS_CREDENTIALS).is_some()
        && arguments.len() == 1
        && !arguments[0].starts_with('-')
    {
        return ExitCode::from(workspace::askpass_main(&arguments[0]).clamp(0, 255) as u8);
    }

    let invocation = match parse_args(arguments) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("gewu-multiverse: {message}\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };

    match invocation {
        Invocation::Help => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Invocation::Version => {
            println!("gewu-multiverse {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Invocation::RunAgent { run_dir } => {
            // One run of one agent, with the plan already written into run_dir. This
            // is the process the supervisor starts; it needs no terminal and no UI.
            let end = agent_run::execute(&run_dir);
            ExitCode::from(end.exit_code.clamp(0, 255) as u8)
        }
        Invocation::GitAskpass { prompt } => {
            // Git is waiting on the other end of the pipe. Write only the answer, and
            // write no error: a failure here must not put a secret into a log.
            ExitCode::from(workspace::askpass_main(&prompt).clamp(0, 255) as u8)
        }
        Invocation::Run(cli) => match run(cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("gewu-multiverse: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Why the program could not start or could not put the terminal back.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    /// The configuration directory could not be decided.
    #[error(transparent)]
    Paths(#[from] gewu_multiverse::paths::PathError),
    /// There is no terminal to draw on.
    #[error(
        "the terminal UI needs a terminal on standard input and standard output, so run it from a shell"
    )]
    NotATerminal,
    /// The terminal could not be put into, or taken out of, the UI mode.
    #[error("the terminal could not be put into or taken out of the UI mode: {0}")]
    Terminal(#[from] io::Error),
}

/// Open the UI and run it until the user leaves.
fn run(cli: Cli) -> Result<(), StartupError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(StartupError::NotATerminal);
    }

    let layout = match cli.root {
        Some(root) => {
            let layout = Layout::rooted_at(root);
            match cli.state_root {
                Some(state) => layout.with_state_root(state),
                None => layout,
            }
        }
        None => {
            let layout = Layout::in_user_config_dir()?;
            match cli.state_root {
                Some(state) => layout.with_state_root(state),
                None => layout,
            }
        }
    };

    let mut app = App::new(layout);
    let mut screen = Screen::enter()?;
    let drawn = event_loop(&mut screen, &mut app);
    // Stop every run before the terminal comes back, and prove the children are gone.
    let stopped = app.shutdown();
    let left = screen.leave();
    drawn.and(left)?;
    if stopped > 0 {
        println!("gewu-multiverse: stopped {stopped} running agent(s) before leaving");
    }
    Ok(())
}

/// Draw, then wait for a key, until the user asks to leave.
fn event_loop(screen: &mut Screen, app: &mut App) -> io::Result<()> {
    loop {
        // Read every live process and refresh the run records before drawing.
        app.tick();
        screen.terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => {
                    // A press is one press: do not act again when the key is released.
                    if key.kind == KeyEventKind::Press {
                        app.on_key(key);
                    }
                }
                // The terminal frames a paste, so a one-line field can keep the first
                // line without joining it to the next, and a repeated paste replaces
                // instead of doubling (P-1, P-2).
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            }
        }
        // Editing a file or reading a log means leaving the UI for a moment and
        // coming back; only this loop can do that.
        if let Some(action) = app.take_action() {
            let result = hand_to_the_user(screen, &action)?;
            app.note_action(&action, result);
        }
    }
}

/// Step out of the UI, hand a path to the user's editor or pager, and step back in.
fn hand_to_the_user(
    screen: &mut Screen,
    action: &PendingAction,
) -> io::Result<io::Result<std::process::ExitStatus>> {
    screen.suspend()?;
    let result = match action.kind {
        ActionKind::Edit => {
            // Say on the primary screen which program is taking over, where it can be
            // read even though the editor's own screen covers the alternate one.
            let program = editor::editor();
            if program.trim() == "vi" {
                println!(
                    "gewu-multiverse: opening {} in vi — press i to type, Esc then :wq to save and quit",
                    action.label
                );
            } else {
                println!("gewu-multiverse: opening {} in {program}", action.label);
            }
            let _ = io::stdout().flush();
            editor::edit(&action.path)
        }
        ActionKind::View => editor::view(&action.path),
    };
    screen.resume()?;
    Ok(result)
}

/// The terminal while the UI is on it, or while it is stepped out of the way.
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
}

impl Screen {
    /// Put the terminal into the UI mode: raw keys and the alternate screen.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(error) = crossterm::execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
        // Take the whole screen, not just the diff: a UI started on a terminal that
        // still holds an earlier program's output (for example after a kill) must not
        // let that text show through the panels as if it were a value.
        terminal.clear()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    /// Step out of the UI: hand the terminal back to the user's own program.
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

    /// Come back into the UI after a program has finished with the terminal.
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
