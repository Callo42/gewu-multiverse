//! The entrypoint: open the terminal, draw the screens, read keys, and put the
//! terminal back exactly as it was.
//!
//! The terminal is restored on every way out, including a panic: [`Screen`] puts it
//! back when it is dropped. Running without a terminal is refused with a sentence,
//! not a panic.

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use gewu_multiverse::app::App;
use gewu_multiverse::cli::{Cli, HELP, Invocation, parse_args};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::ui;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// How long the loop waits for a key before drawing again.
const TICK: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let invocation = match parse_args(std::env::args().skip(1)) {
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
        Some(root) => Layout::rooted_at(root),
        None => Layout::in_user_config_dir()?,
    };

    let mut app = App::new(layout);
    let mut screen = Screen::enter()?;
    let drawn = event_loop(&mut screen, &mut app);
    let left = screen.leave();
    drawn.and(left)?;
    Ok(())
}

/// Draw, then wait for a key, until the user asks to leave.
fn event_loop(screen: &mut Screen, app: &mut App) -> io::Result<()> {
    loop {
        screen.terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                // A press is one press: do not act again when the key is released.
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
    }
}

/// The terminal while the UI is on it.
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
}

impl Screen {
    /// Put the terminal into the UI mode: raw keys and the alternate screen.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(error) = crossterm::execute!(out, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let terminal = Terminal::new(CrosstermBackend::new(out))?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    /// Put the terminal back. Safe to call more than once.
    fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        disable_raw_mode()?;
        crossterm::execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
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
