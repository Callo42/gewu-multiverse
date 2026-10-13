//! Drawing the screens.
//!
//! [`draw`] is a plain function of [`App`] and a frame, so the same code paints the
//! real terminal and, in the tests, an in-memory backend: what the tests show is what
//! the terminal shows.
//!
//! Every screen that has nothing behind it says so, in a line that starts with
//! `Placeholder`. No screen offers an action the program cannot carry out: the state
//! it shows comes from the supervisor, the files it names are read back from disk,
//! and what is still missing — the model call and the Forgejo work — is named.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

use crate::app::{App, Screen};
use crate::risk;
use crate::run::RunOutcome;
use crate::systemd::Mode;

/// The space between the field name and its value on the detail screens.
const LABEL_WIDTH: usize = 16;

/// Paint the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(4),
    ])
    .split(frame.area());

    draw_tabs(frame, app, areas[0]);
    draw_body(frame, app, areas[1]);
    draw_footer(frame, app, areas[2]);

    if app.show_acknowledgment {
        draw_acknowledgment(frame, app);
    }
}

/// The tab bar across the top.
fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Screen::ALL
        .iter()
        .map(|screen| Line::from(screen.title()))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.screen.index())
        .divider(" ")
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::bordered().title(" gewu-multiverse "));
    frame.render_widget(tabs, area);
}

/// The body of the screen on show.
fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    match app.screen {
        Screen::Agents => draw_agents(frame, app, area),
        Screen::AgentConfig => draw_agent_config(frame, app, area),
        Screen::Instructions => draw_instructions(frame, app, area),
        Screen::Forgejo => draw_forgejo(frame, app, area),
        Screen::Credentials => draw_credentials(frame, app, area),
        Screen::Runs => draw_runs(frame, app, area),
    }
}

/// The list of agents and their state.
fn draw_agents(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).split(area);

    let header = Row::new(["agent", "model", "requests", "supervision", "state"]).style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows: Vec<Row> = app
        .agents()
        .iter()
        .enumerate()
        .map(|(at, agent)| {
            let style = if at == app.selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(agent.name.clone()),
                Cell::from(agent.model.clone()),
                Cell::from(agent.concurrency.to_string()),
                Cell::from(app.supervisor.mode(&agent.name).as_str()),
                Cell::from(app.supervisor.state(&agent.name).word()),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Min(18),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(" agents "));
    frame.render_widget(table, parts[0]);

    frame.render_widget(
        Paragraph::new(agent_notes(app)).wrap(Wrap { trim: true }),
        parts[1],
    );
}

/// The lines under the agent list.
fn agent_notes(app: &App) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(error) = &app.config_error {
        lines.push(Line::from(Span::styled(
            format!("no agent list: {error}"),
            Style::new().fg(Color::Red),
        )));
    }
    lines.push(Line::from(format!(
        "Running: {} of {} allowed by the configuration; {} start(s) waiting for a slot.",
        app.supervisor.running_count(),
        app.supervisor.max_concurrent(),
        app.supervisor.waiting_count(),
    )));
    if let Some(error) = app.supervisor.last_error() {
        lines.push(Line::from(Span::styled(
            format!("the last start did not go well: {error}"),
            Style::new().fg(Color::Red),
        )));
    }
    if let Some(error) = app.supervisor.systemd_last_error() {
        lines.push(Line::from(Span::styled(
            format!("the last look at the units did not go well: {error}"),
            Style::new().fg(Color::Red),
        )));
    }
    if app
        .agents()
        .iter()
        .any(|agent| app.supervisor.mode(&agent.name) == Mode::Systemd)
    {
        lines.push(Line::from(Span::styled(
            format!(
                "A `systemd` agent runs as a user unit: it keeps running when this UI exits. Your systemd directory is {}. It needs a user session, and `loginctl enable-linger` to outlive a logout. It is a convenience, not a security boundary.",
                app.supervisor.unit_dir().display()
            ),
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(Span::styled(
        "An agent in `tui` mode is an ordinary child process of this tool, run as you. Either way its workspace is a directory: a layout, not a sandbox.",
        Style::new().fg(Color::DarkGray),
    )));
    if !app.ack_confirmed() {
        lines.push(Line::from(Span::styled(
            "The risk of running an agent is not acknowledged: starting one is refused.",
            Style::new().fg(Color::Yellow),
        )));
    }
    Text::from(lines)
}

/// One agent's configuration, read from the configuration file and the supervisor.
fn draw_agent_config(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("name", &agent.name));
            lines.push(field("model", &agent.model));
            lines.push(field("endpoint", &agent.endpoint));
            lines.push(field("max turns", &agent.max_turns.to_string()));
            lines.push(field(
                "instruction",
                &agent.instruction.display().to_string(),
            ));
            lines.push(field("requests", &agent.concurrency.to_string()));
            lines.push(Line::from(""));
            lines.push(field(
                "run state",
                &app.supervisor.state(&agent.name).word(),
            ));
            let mode = app.supervisor.mode(&agent.name);
            lines.push(field("supervision", &format!("{} ({})", mode.label(), mode.as_str())));
            if mode == Mode::Systemd {
                lines.push(field(
                    "unit",
                    &crate::systemd::unit_name(&agent.name),
                ));
                lines.push(field(
                    "restarts",
                    &app.supervisor.restart_count(&agent.name).to_string(),
                ));
            }
            if let Some(paths) = app.selected_paths() {
                lines.push(field(
                    "agent files",
                    &paths.config_dir().display().to_string(),
                ));
                lines.push(field("workspace", &paths.workspace().display().to_string()));
            }
            if let Some(problem) = app.selected_paths_problem() {
                lines.push(Line::from(Span::styled(
                    problem,
                    Style::new().fg(Color::Red),
                )));
            }
            match app.latest_run_log(&agent.name) {
                Some(log) => lines.push(field("newest log", &log.display().to_string())),
                None => lines.push(field("newest log", "no run yet")),
            }
            if let Some(run_id) = app.supervisor.state(&agent.name).run_id() {
                lines.push(field("run", run_id));
            }
            if let Some(test) = &app.endpoint_test {
                if test.agent == agent.name {
                    lines.push(field("endpoint test", &test.describe()));
                }
            }
            match crate::credentials::recorded_login(&app.credentials_path, &agent.name) {
                Ok(Some(login)) => lines.push(field("forgejo identity", &login)),
                _ => lines.push(field("forgejo identity", "not settled — press i")),
            }
            if let Some(report) = &app.identity_report {
                if report.agent == agent.name {
                    lines.push(field("forgejo account", &report.describe()));
                }
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(field("read from", &app.config_path().display().to_string()));
    lines.push(Line::from(Span::styled(
        "e edit the instruction   K open the skills directory   t edit the declared steps   p test the endpoint   s start   x stop   m switch supervision mode   o open the log",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" agent configuration "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// An agent's instruction, its declared steps and its skills, as they are on disk.
fn draw_instructions(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("agent", &agent.name));
            match app.instruction_summary() {
                Some((path, exists, bytes)) => {
                    lines.push(field("instruction", &path.display().to_string()));
                    lines.push(field(
                        "  on disk",
                        &if exists {
                            format!("{bytes} bytes")
                        } else {
                            "not there yet — press e to write it".to_owned()
                        },
                    ));
                }
                None => lines.push(field("instruction", "the name cannot be used")),
            }
            match app.steps_summary() {
                Some((path, exists, count)) => {
                    lines.push(field("steps", &path.display().to_string()));
                    lines.push(field(
                        "  declared",
                        &if exists {
                            format!("{count} shell step(s), run in the workspace")
                        } else {
                            "none — press t to declare some".to_owned()
                        },
                    ));
                }
                None => lines.push(field("steps", "the name cannot be used")),
            }
            let skills = app.skills_summary();
            lines.push(field("skills", &format!("{} on disk", skills.len())));
            for (name, bytes) in skills.iter().take(8) {
                lines.push(Line::from(format!(
                    "                  {name} (SKILL.md, {bytes} bytes)"
                )));
            }
            if skills.len() > 8 {
                lines.push(Line::from(format!(
                    "                  … and {} more",
                    skills.len() - 8
                )));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "e edit the instruction   K open the skills directory   t edit the declared steps",
        Style::new().fg(Color::DarkGray),
    )));
    if let Some(agent) = app.selected_agent() {
        lines.push(Line::from(Span::styled(
            format!(
                "A run sends this instruction, the conversation so far and the declared `shell` tool to {} ({}), runs the tool calls it asks for inside the workspace, and stops when the model finishes or after {} turn(s).",
                agent.endpoint, agent.model, agent.max_turns
            ),
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" instruction, steps and skills "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The Forgejo connection, the role check and the repository list.
fn draw_forgejo(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match &app.config {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(config) => {
            lines.push(field("url", &config.forgejo.url));
            lines.push(field("role", config.forgejo.role.as_str()));
        }
    }
    let (member, admin) = app.forgejo_tokens_present();
    lines.push(field(
        "role token",
        if member {
            "in the credentials file"
        } else {
            "absent"
        },
    ));
    lines.push(field(
        "admin token",
        if admin {
            "configured (identities are managed)"
        } else {
            "absent (identities are supplied)"
        },
    ));
    if let Some(agent) = app.selected_agent() {
        match crate::credentials::recorded_login(&app.credentials_path, &agent.name) {
            Ok(Some(login)) => lines.push(field("agent identity", &login)),
            _ => lines.push(field("agent identity", "not settled — press i")),
        }
    }
    lines.push(Line::from(""));

    match &app.role_check {
        None => lines.push(placeholder(
            "no role check has been run yet — press c to list what the configured token can see",
        )),
        Some(Err(failure)) => lines.push(Line::from(Span::styled(
            format!("role check failed ({}): {}", failure.class, failure.message),
            Style::new().fg(Color::Red),
        ))),
        Some(Ok(check)) => {
            lines.push(Line::from(format!(
                "{} — {} ({} of these private)",
                if check.administrative() {
                    "administrative view"
                } else {
                    "ordinary view"
                },
                check.summary(),
                check.private_visible
            )));
            lines.push(Line::from(""));
            if check.repositories.is_empty() {
                lines.push(Line::from(
                    "the token can see no repository on this instance",
                ));
            }
            for (at, repository) in check.repositories.iter().enumerate() {
                let style = if at == app.repo_selected {
                    Style::new().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::new()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}", repository.line()),
                    style,
                )));
            }
        }
    }
    if let Some(report) = &app.identity_report {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "identity for `{}`: {}",
            report.agent,
            report.describe()
        )));
    }
    if let Some(note) = &app.forgejo_note {
        lines.push(Line::from(note.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "c check the role   i settle the agent's identity   up/down choose a repository   g clone it into the agent's workspace   P open a pull request as the agent   U update the newest one",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" Forgejo connection and role check "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Where the credentials file is, and the mode it has.
fn draw_credentials(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(field("file", &app.credentials_path.display().to_string()));
    lines.push(field("mode", &mode_text(app)));
    lines.push(Line::from(""));
    match &app.credentials_error {
        None => lines.push(Line::from(
            "The file is created with mode 0600 if it is absent, and held to 0600 if it is wider.",
        )),
        Some(error) => lines.push(Line::from(Span::styled(
            format!("the credentials file is not ready: {error}"),
            Style::new().fg(Color::Red),
        ))),
    }
    lines.push(Line::from(
        "Its contents are never printed: nothing here is shown, logged or put on a command line.",
    ));
    lines.push(Line::from(Span::styled(
        "An agent runs as you, so it can read this file with your rights. The screen shows you where it is.",
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "The model key for an agent is read from this file at the point of a model call, and is redacted from every transcript, log and error the tool writes.",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" credentials file "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The recent runs, read from the records on disk.
fn draw_runs(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).split(area);

    let header = Row::new(["agent", "run", "started", "outcome"]).style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows: Vec<Row> = app
        .recent
        .iter()
        .enumerate()
        .map(|(at, run)| {
            let style = if at == app.run_selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if run.outcome == Some(RunOutcome::Failed) {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(run.agent.clone()),
                Cell::from(run.run_id.clone()),
                Cell::from(run.started_text()),
                Cell::from(run.describe()),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Min(18),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(" recent runs "));
    frame.render_widget(table, parts[0]);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!(
        "Run records are read from {}.",
        app.state_root().display()
    )));
    if app.recent.is_empty() {
        lines.push(Line::from(
            "No run has been recorded yet, so there is nothing to list.",
        ));
    } else if let Some(run) = app.selected_run() {
        lines.push(Line::from(format!(
            "o opens the log on show: {}",
            run.log.display()
        )));
        lines.push(Line::from(Span::styled(
            "The outcome is decided by the process, not by what it wrote.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" run records "))
            .wrap(Wrap { trim: true }),
        parts[1],
    );
}

/// The status line and the key hints.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let status_style = if app.config_error.is_some() || app.credentials_error.is_some() {
        Style::new().fg(Color::Red)
    } else if !app.ack_confirmed() {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(app.status.as_str())
            .style(status_style)
            .wrap(Wrap { trim: true }),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(
            "q quit   tab or left/right screen   1-6 screen   up/down select   enter open   esc agents   r re-read",
        )
        .style(Style::new().fg(Color::DarkGray))
        .wrap(Wrap { trim: true }),
        parts[1],
    );
    frame.render_widget(
        Paragraph::new(
            "s start the agent   x stop it   m switch supervision mode (tui / systemd)   p test its endpoint   c check the Forgejo role   i settle its Forgejo identity   e edit its instruction   K open its skills directory   t edit its declared steps   o open its newest log",
        )
        .style(Style::new().fg(Color::DarkGray))
        .wrap(Wrap { trim: true }),
        parts[2],
    );
}

/// The first-run risk acknowledgment, over everything else.
fn draw_acknowledgment(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    let width = 78.min(screen.width);
    let height = (risk::LINES.len() as u16 + 9).min(screen.height);
    let popup = centered(screen, width, height);

    let mut lines: Vec<Line> = Vec::new();
    for text in risk::LINES {
        lines.push(Line::from(vec![Span::raw("  * "), Span::raw(*text)]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  y — I understand, record it once        n — not now",
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(
        "  Until it is recorded, starting an agent stays refused.",
    ));
    lines.push(Line::from(format!(
        "  The record will be written to {}",
        app.ack_path.display()
    )));

    let block = Block::bordered()
        .title(format!(" {} ", risk::TITLE))
        .border_style(Style::new().fg(Color::Yellow));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// A centered rectangle inside `area`, never larger than it.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// A line naming a field and its value.
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<LABEL_WIDTH$}"),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw(value.to_owned()),
    ])
}

/// A line that says plainly that something is not there yet.
fn placeholder(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("Placeholder — {text}."),
        Style::new().fg(Color::Yellow),
    ))
}

/// Why there is no agent to show.
fn no_agent_text(app: &App) -> String {
    match &app.config_error {
        Some(error) => format!("no agent to show: {error}"),
        None => "no agent to show: the configuration lists none".to_owned(),
    }
}

/// The credentials file's mode, as read back from disk.
fn mode_text(app: &App) -> String {
    match app.credentials_mode {
        Some(mode) => crate::mode::render(mode),
        None => "not read".to_owned(),
    }
}
