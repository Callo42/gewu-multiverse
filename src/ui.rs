//! Drawing the screens.
//!
//! [`draw`] is a plain function of [`App`] and a frame, so the same code paints the
//! real terminal and, in the tests, an in-memory backend: what the tests show is what
//! the terminal shows.
//!
//! Every screen that has nothing behind it says so, in a line that starts with
//! `Placeholder`. No screen offers an action the program cannot carry out.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

use crate::app::{App, Screen};
use crate::risk;

/// The space between the field name and its value on the detail screens.
const LABEL_WIDTH: usize = 16;

/// Paint the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(3),
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
    let parts = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(area);

    let header = Row::new(["agent", "model", "concurrency", "state"]).style(
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
                Cell::from("defined; never run"),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(22),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Min(16),
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
    lines.push(placeholder(
        "no supervisor exists yet, so nothing here starts, stops or watches an agent",
    ));
    if !app.ack_confirmed() {
        lines.push(Line::from(Span::styled(
            "The risk of running an agent is not acknowledged: starting one is refused.",
            Style::new().fg(Color::Yellow),
        )));
    }
    Text::from(lines)
}

/// One agent's configuration, read from the configuration file.
fn draw_agent_config(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("name", &agent.name));
            lines.push(field("model", &agent.model));
            lines.push(field("endpoint", &agent.endpoint));
            lines.push(field(
                "instruction",
                &agent.instruction.display().to_string(),
            ));
            lines.push(field("concurrency", &agent.concurrency.to_string()));
        }
    }
    lines.push(Line::from(""));
    lines.push(field("read from", &app.config_path().display().to_string()));
    lines.push(placeholder(
        "editing an agent from here is not implemented; change the configuration file and press r",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" agent configuration "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// An agent's instruction and skills.
fn draw_instructions(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("agent", &agent.name));
            lines.push(field(
                "instruction",
                &agent.instruction.display().to_string(),
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "The file above is not read or shown yet, and no skill set is installed.",
                Style::new().fg(Color::Yellow),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(placeholder(
        "instruction handling and skill sets are not implemented; nothing is delivered to an agent",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" instruction and skills "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The Forgejo connection and the role check.
fn draw_forgejo(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match &app.config {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(config) => {
            lines.push(field("url", &config.forgejo.url));
            lines.push(field("role", config.forgejo.role.as_str()));
        }
    }
    lines.push(Line::from(""));
    lines.push(placeholder(
        "the role check is not implemented: no request has been sent and no repository has been looked at",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" Forgejo connection "))
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
        "Its contents are never read here: nothing is printed, logged or put on a command line.",
    ));
    lines.push(placeholder(
        "no secret is used yet, because no agent runs and no request is made",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" credentials file "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Each agent's recent runs.
fn draw_runs(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if app.agents().is_empty() {
        lines.push(Line::from(no_agent_text(app)));
    }
    for agent in app.agents() {
        lines.push(Line::from(format!(
            "  {} — runs recorded: 0, and no transcript to show",
            agent.name
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "No agent has ever run, so there is nothing else to list here.",
    ));
    lines.push(placeholder(
        "run records, transcripts and logs are not implemented",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" recent runs, one line per agent "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The status line and the key hints.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // Two rows for the status, so a whole message fits, and one for the keys.
    let parts = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).split(area);
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
            "q quit   tab or left/right screen   1-6 screen   up/down agent   enter open   esc agents   s start   r re-read",
        )
        .style(Style::new().fg(Color::DarkGray))
        .wrap(Wrap { trim: true }),
        parts[1],
    );
}

/// The first-run risk acknowledgment, over everything else.
fn draw_acknowledgment(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    let width = 78.min(screen.width);
    let height = (risk::LINES.len() as u16 + 8).min(screen.height);
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
