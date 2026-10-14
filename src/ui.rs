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

use crate::app::{App, NewAgentForm, Screen};
use crate::identity;
use crate::risk;
use crate::run::RunOutcome;
use crate::systemd::Mode;

/// The space between the field name and its value on the detail screens.
const LABEL_WIDTH: usize = 16;

/// The smallest window the product will draw its panels into.
///
/// Below this, panels and the modal clip silently — an action line or a reason simply
/// disappears. One honest line is better than a clipped screen, and it says exactly
/// what to do.
const MIN_WIDTH: u16 = 72;
const MIN_HEIGHT: u16 = 18;

/// Paint the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let full = frame.area();
    if full.width < MIN_WIDTH || full.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "this window is too small to show the product; make it at least {MIN_WIDTH}×{MIN_HEIGHT}"
            ))
            .wrap(Wrap { trim: true }),
            full,
        );
        return;
    }
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(5),
    ])
    .split(frame.area());

    draw_tabs(frame, app, areas[0]);
    draw_body(frame, app, areas[1]);
    draw_footer(frame, app, areas[2]);

    if app.adding_agent() {
        draw_add_agent_form(frame, app);
    } else if app.editor_confirm.is_some() {
        draw_editor_confirm(frame, app);
    } else if app.secret_prompt.is_some() {
        draw_secret_prompt(frame, app);
    } else if app.confirm_remove.is_some() {
        draw_remove_confirm(frame, app);
    } else if app.show_help {
        draw_help(frame, app);
    }
    // The first-run acknowledgment is the topmost overlay: nothing starts behind it.
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
    // The note area carries the words the list uses, the failure reason and the
    // supervision explanation; give it the rows to say all of it without clipping —
    // but never at the cost of the list itself. The table keeps the four rows it
    // needs for its border, its header and **at least one agent**, even at the
    // smallest size the product calls usable, so a configured agent is never
    // invisible.
    let notes_height = area.height.saturating_sub(4).min(8);
    let parts =
        Layout::vertical([Constraint::Min(4), Constraint::Length(notes_height)]).split(area);

    let capacity = parts[0].height.saturating_sub(3) as usize;
    let (first, shown) = visible_window(app.agents().len(), app.selected, capacity);

    let header = Row::new(["", "agent", "model", "concurrency", "supervision", "state"])
        .style(
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let rows: Vec<Row> = app
        .agents()
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, agent)| {
            let style = if at == app.selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::new()
            };
            Row::new(vec![
                // The selected row carries a textual marker as well as colour: a
                // monochrome capture, or a person who cannot tell the colours apart,
                // still sees which row the next key will act on.
                Cell::from(if at == app.selected { ">" } else { " " }),
                Cell::from(agent.name.clone()),
                Cell::from(agent.model.clone()),
                Cell::from(agent.concurrency.to_string()),
                Cell::from(app.supervisor.mode(&agent.name).as_str()),
                Cell::from(app.supervisor.state(&agent.name).word_with_reason()),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Min(18),
    ];
    let table = Table::new(rows, widths).header(header).block(
        Block::bordered().title(list_title("agents", first, shown, app.agents().len())),
    );
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
    if let Some(note) = &app.config_note {
        lines.push(Line::from(Span::styled(
            note.clone(),
            Style::new().fg(Color::Yellow),
        )));
    }
    // Nothing configured: the product itself offers the way forward, by name.
    if app.agents().is_empty() && app.config_error.is_none() {
        lines.push(Line::from(Span::styled(
            "No agent is configured yet — press n to name one, give it an endpoint and a model, \
             and the product writes both files for you.",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(format!(
        "Running: {} of {} allowed by the configuration; {} start(s) waiting for a slot.",
        app.supervisor.running_count(),
        app.supervisor.max_concurrent(),
        app.supervisor.waiting_count(),
    )));
    // Why the selected agent's last run failed, on the screen the user looks at.
    if let Some(name) = app.selected_name() {
        if let Some(reason) = app.last_failure(&name) {
            lines.push(Line::from(Span::styled(
                format!("the last run of `{name}` did not go well: {reason}"),
                Style::new().fg(Color::Red),
            )));
        }
    }
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
    // The two numbers the list and the Agent screen show are defined where they first
    // appear, so a zero-knowledge user is not left with an undefined word.
    lines.push(Line::from(Span::styled(
        "concurrency is how many model requests one agent may keep in flight at once; max turns is the most model turns one run may take.",
        Style::new().fg(Color::DarkGray),
    )));
    // Explain what `systemd` will change *before* `m` is pressed, not only in
    // the receipt afterwards: whether any agent is in systemd mode or not.
    let uses_systemd = app
        .agents()
        .iter()
        .any(|agent| app.supervisor.mode(&agent.name) == Mode::Systemd);
    lines.push(Line::from(Span::styled(
        format!(
            "The `tui` mode is an ordinary child process of this tool; `m` switches to `systemd`, a user unit that keeps running when this UI exits and needs `loginctl enable-linger` to outlive a logout. Your systemd directory is {}.",
            app.supervisor.unit_dir().display()
        ),
        Style::new().fg(Color::DarkGray),
    )));
    if uses_systemd {
        lines.push(Line::from(Span::styled(
            "A `systemd` agent is supervised by your own systemd. It is a convenience, not a security boundary.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Either way its workspace is a directory: a layout, not a sandbox.",
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
            lines.push(field("concurrency", &agent.concurrency.to_string()));
            lines.push(Line::from(""));
            lines.push(field(
                "run state",
                &app.supervisor.state(&agent.name).word_with_reason(),
            ));
            if let Some(reason) = app.last_failure(&agent.name) {
                lines.push(field("last failure", &reason));
            }
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
                // A `systemd` record whose user manager cannot be reached is the T-1
                // state, and it must be visible where the mode is shown: say what is
                // missing, and that `m` leaves the mode without needing the manager
                // when no unit file is on disk.
                if let Some(reason) = app.supervisor.systemd_last_error() {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "the user's systemd cannot be reached: {reason} — press m to leave systemd mode; leaving needs no unit on disk and no manager"
                        ),
                        Style::new().fg(Color::Red),
                    )));
                }
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
                Ok(Some(login)) => lines.push(field("identity", &login)),
                _ => lines.push(field("identity", "not settled — press i")),
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
        "e edit the instruction   K start a new skill   t edit the declared steps   p test the endpoint   s start   x stop   m switch supervision mode   o open the log",
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
                        &if !exists {
                            "not there yet — press e to write it".to_owned()
                        } else if app.instruction_is_only_placeholder() {
                            "not there yet — press e to write it; the file holds only the guidance line"
                                .to_owned()
                        } else {
                            // What a run sends, not only what is on disk: the
                            // guidance line is dropped, and the two numbers differ
                            // (P-4).
                            match app.instruction_payload() {
                                Some((on_disk, sent)) if on_disk != sent => format!(
                                    "{on_disk} bytes on disk; a run sends {sent} — the guidance line is dropped"
                                ),
                                Some((on_disk, _)) => format!("{on_disk} bytes"),
                                None => format!("{bytes} bytes"),
                            }
                        },
                    ));
                    // An interrupted editor leaves its unsaved buffer beside the file;
                    // naming it is the difference between recovery and retyping.
                    if let Some(backup) = app.instruction_backup() {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "an earlier edit of this instruction was not saved; the editor kept it in {} — open that file to recover",
                                backup.display()
                            ),
                            Style::new().fg(Color::Yellow),
                        )));
                    }
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
        "e edit the instruction   K start a new skill   t edit the declared steps",
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "A skill is one directory under skills/, each holding a SKILL.md; `K` opens a scratch one \
         you can keep or rename. A run installs the skills it finds into the run directory.",
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
            Ok(Some(login)) => {
                // The mode is shown persistently, beside the login, not only in the
                // transient status line: which account a clone or a pull request acts
                // as is exactly the thing a user with several tokens needs to know
                // before pressing the key, not after.
                let mode = identity::recorded_mode(&app.credentials_path)
                    .ok()
                    .flatten()
                    .map(|mode| format!(" ({})", mode.as_str()))
                    .unwrap_or_default();
                lines.push(field(
                    "agent identity",
                    &format!("{login}{mode} — clones and pull requests act as this account"),
                ));
            }
            _ => lines.push(field("agent identity", "not settled — press i")),
        }
    }
    lines.push(Line::from(Span::styled(
        ROLE_EXPLANATION,
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    let mut repo_window: Option<(usize, usize, usize)> = None;
    match &app.role_check {
        None => lines.push(placeholder(
            "no role check has been run yet — press c to list what the configured token can see",
        )),
        Some(Err(failure)) => {
            // P-7: a refusal names the URL the user configured, not only the API path,
            // so a mistyped path connects to what was typed.
            let at = app
                .config
                .as_ref()
                .map(|config| config.forgejo.url.clone())
                .unwrap_or_else(|| "the configured address".to_owned());
            lines.push(Line::from(Span::styled(
                format!(
                    "role check failed ({}) at {at}: {}",
                    failure.class, failure.message
                ),
                Style::new().fg(Color::Red),
            )))
        }
        Some(Ok(check)) => {
            let summary = format!(
                "{} — {}",
                if check.administrative() {
                    "administrative view"
                } else {
                    "ordinary view"
                },
                check.summary()
            );
            lines.push(Line::from(summary.clone()));
            lines.push(Line::from(""));
            if check.repositories.is_empty() {
                lines.push(Line::from(
                    "the token can see no repository on this instance",
                ));
            } else {
                // The repository list takes the Agents/Runs rule, not a rule of its
                // own: the window is chosen so the selected repository is always
                // drawn, and the panel title says which rows are on show. Before
                // this, at 80x24 a repository was dropped with no range and no
                // `more`, and the selection walked onto the undrawn row (W-1).
                let inner = area.width.saturating_sub(2) as usize;
                let content_height = area.height.saturating_sub(2) as usize;
                let used_before = lines.len()
                    + wrapped_rows(ROLE_EXPLANATION, inner).saturating_sub(1)
                    + wrapped_rows(&summary, inner).saturating_sub(1);
                let capacity = content_height.saturating_sub(used_before).max(1);
                let total = check.repositories.len();
                let (first, shown) = visible_window(total, app.repo_selected, capacity);
                for (at, repository) in check
                    .repositories
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(shown)
                {
                    let style = if at == app.repo_selected {
                        Style::new().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::new()
                    };
                    // The selected row is marked in text as well as colour: a
                    // monochrome capture, or a person who cannot tell the colours
                    // apart, still sees which repository `g` and `P` will act on.
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} {}",
                            if at == app.repo_selected { '>' } else { ' ' },
                            repository.line()
                        ),
                        style,
                    )));
                }
                repo_window = Some((first, shown, total));
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
        "c check the role   C check it as the administrator token   v set the agent's own token   A set the administrator token   i settle the agent's identity   up/down choose a repository   g clone it into the agent's workspace   P open a pull request as the agent   U update the newest one",
        Style::new().fg(Color::DarkGray),
    )));
    let mut title = " Forgejo connection and role check ".to_owned();
    if let Some((first, shown, total)) = repo_window {
        if total > shown {
            title = format!(
                " Forgejo connection and role check —{}",
                list_title("repositories", first, shown, total)
            );
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The one sentence that explains what a role is and what an identity is.
///
/// It is a constant because the repository window has to measure it: the rows this
/// sentence wraps onto are rows the repository list cannot use (W-1).
const ROLE_EXPLANATION: &str = "A role is which Forgejo account the check acts as: `c` checks the configured role, `C` checks the administrator token when one is present. An identity is the account an agent's own commits are authored by; `i` creates or records it.";

/// How many rows a single plain string needs when a panel wraps at `width`.
///
/// The repository window needs to know how much of the panel the text above it
/// takes. This is the same greedy, word-boundary wrapping the panel uses; it is used
/// only to *reserve* rows, so a small underestimate costs a list row, never a
/// silent one — the window and the title still agree.
fn wrapped_rows(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1usize;
    let mut current = 0usize;
    for word in text.split(' ') {
        let len = word.chars().count();
        if current == 0 {
            current = len;
        } else if current + 1 + len <= width {
            current += 1 + len;
        } else {
            rows += 1;
            current = len;
        }
        while current > width {
            rows += 1;
            current -= width;
        }
    }
    rows
}

/// Where the credentials file is, and the mode it has.
fn draw_credentials(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(field("file", &app.credentials_path.display().to_string()));
    lines.push(field("mode", &mode_text(app)));
    // Which secret *names* are present, never a value: a person can confirm a key
    // loaded without the tool reading it back onto a screen.
    let present = app.present_secret_labels();
    if let Some(problem) = crate::credentials::parse_problem(&app.credentials_path) {
        // A file that could not be read is not an empty file. Say which it is.
        lines.push(Line::from(Span::styled(
            format!("the credentials file is not ready: {problem}"),
            Style::new().fg(Color::Red),
        )));
    } else if present.is_empty() {
        lines.push(field(
            "present",
            "nothing yet — no model key and no Forgejo token",
        ));
    } else {
        lines.push(field("present", &present.join(", ")));
    }
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
    lines.push(Line::from(Span::styled(
        "k set the model key   f set the Forgejo role token   A set the administrator token",
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
    // The same rule as the agent list: never silently drop a run, and never let the
    // selection land on a row that is not drawn.
    let notes_height = area.height.saturating_sub(4).min(9);
    let parts =
        Layout::vertical([Constraint::Min(4), Constraint::Length(notes_height)]).split(area);

    let capacity = parts[0].height.saturating_sub(3) as usize;
    let (first, shown) = visible_window(app.recent.len(), app.run_selected, capacity);

    // The start time is written in RFC 3339 UTC on disk; saying so in the header is
    // what stops it reading as a wrongly-set wall clock (W-4).
    let header = Row::new(["", "agent", "run", "started (UTC)", "outcome"]).style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows: Vec<Row> = app
        .recent
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, run)| {
            let style = if at == app.run_selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if run.outcome == Some(RunOutcome::Failed) {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(if at == app.run_selected { ">" } else { " " }),
                Cell::from(run.agent.clone()),
                Cell::from(run.run_id.clone()),
                Cell::from(run.started_text()),
                Cell::from(run.describe()),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(12),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Min(18),
    ];
    let table = Table::new(rows, widths).header(header).block(
        Block::bordered().title(list_title("recent runs", first, shown, app.recent.len())),
    );
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
        // A person who came here to see whether anything ran needs the step that makes
        // a run, not only where the records would appear.
        lines.push(Line::from(
            "Press s on the Agents screen (1) to start one.",
        ));
    } else if let Some(run) = app.selected_run() {
        // The reason leads, because it is what a person who returns to this screen
        // came for: the page map asks the finished row for the outcome *with its
        // reason*. The paths follow; if the note area is short, the paths are what
        // wrap off the bottom, never the reason.
        if let Some(reason) = run.reason.as_deref() {
            lines.push(Line::from(format!("outcome: {} — {reason}", run.describe())));
        } else {
            lines.push(Line::from(format!("outcome: {}", run.describe())));
        }
        // The transcript is the run's own record; `T` opens it here, never only from a
        // path the README happens to name.
        lines.push(Line::from(format!(
            "T opens the transcript on show: {}",
            run.dir.join(crate::transcript::TRANSCRIPT_FILE).display()
        )));
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
///
/// The status gets two rows so a path or a reason is not clipped. The action
/// line leads with `? all keys`, so at 80 columns nothing is hidden *silently*;
/// the full list is one key away.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // The status gets three rows so a long reason or path wraps instead of being cut;
    // the legend gets its own row and is built to fit it, tail and all.
    let parts = Layout::vertical([
        Constraint::Length(3),
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
        Paragraph::new(key_legend(
            area.width as usize,
            &running_clause(app),
        ))
        .style(Style::new().fg(Color::DarkGray)),
        parts[2],
    );
}

/// The short clause the footer carries on every screen.
///
/// Two answers a person is owed wherever they are: what is running now, and what has
/// finished since this UI started — the questions a user who leaves a screen and comes
/// back asks first.
fn running_clause(app: &App) -> String {
    let base = format!(
        "Running: {} of {}   ",
        app.supervisor.running_count(),
        app.supervisor.max_concurrent()
    );
    let finished = app.runs_finished_since_start();
    if finished == 0 {
        base
    } else {
        format!("{base}· {finished} finished   ")
    }
}

/// The action keys, built to fit the width it is drawn into.
///
/// The legend must not stop mid-list with no sign that more exists: it shows as many
/// keys as fit and, when one does not, ends with a fixed tail naming the full list —
/// the page map's own rule. On a wide terminal every key fits and the line ends with
/// `? all keys` too. The `prefix` is the running count, which is rendered here so that
/// it is on screen wherever the user is, not only on the Agents screen.
fn key_legend(width: usize, prefix: &str) -> String {
    const KEYS: &[&str] = &[
        "s start",
        "x stop",
        "p endpoint",
        "m supervision",
        "c role",
        "C admin role",
        "i identity",
        "e instruction",
        "t steps",
        "K skill",
        "o log",
        "T transcript",
        "n new agent",
        "d remove",
    ];
    let tail = "… more — ? all keys";
    // If the count would crowd out even the full-list hint, the hint wins: it is the
    // affordance the page map requires, and the count is repeated on the Agents screen.
    if width <= tail.chars().count() {
        return "? all keys".to_owned();
    }
    if prefix.chars().count() + tail.chars().count() > width {
        return tail.to_owned();
    }
    let mut line = prefix.to_owned();
    for key in KEYS {
        let candidate = format!("{line}{key}   ");
        if candidate.chars().count() + tail.chars().count() > width {
            line.push_str(tail);
            return line;
        }
        line = candidate;
    }
    let candidate = format!("{line}? all keys");
    if candidate.chars().count() <= width {
        candidate
    } else {
        line.push_str(tail);
        line
    }
}

/// The add-agent form, over the Agents screen.
///
/// The two secret fields are drawn masked: a credential value is never put on a
/// screen, and the form exists so the user never has to open a text editor.
fn draw_add_agent_form(frame: &mut Frame, app: &App) {
    let Some(form) = &app.add_agent else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let height = (NewAgentForm::FIELDS.len() as u16 + 10).min(screen.height);
    let popup = centered(screen, width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            y: popup.y,
            width: screen.width,
            height: popup.height,
        },
    );

    let mut lines: Vec<Line> = vec![
        Line::from(
            "Name an agent and point it at a model. The model key and the Forgejo token are written to the credentials file and are never shown.",
        ),
        Line::from(Span::styled(
            "The model key is required. The Forgejo fields are optional; a blank URL keeps the sample's placeholder address.",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "The Forgejo token here is the role token — the account the role check acts as. An agent's own token is a different secret: supply it later with `v` on the Forgejo screen, where `i` needs it.",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];
    for (at, (label, secret)) in NewAgentForm::FIELDS.iter().enumerate() {
        let value = &form.fields[at];
        // A masked value says how much it holds: the mask alone is the same block of
        // stars whether a paste landed once or twice (P-2).
        let shown = if *secret {
            let count = value.chars().count();
            if count == 0 {
                String::new()
            } else {
                format!("{} ({count} characters, hidden)", "*".repeat(count.min(12)))
            }
        } else {
            value.clone()
        };
        let marker = if at == form.cursor { ">" } else { " " };
        let style = if at == form.cursor {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {label:<16} {shown}"),
            style,
        )));
    }
    lines.push(Line::from(""));
    match &form.error {
        Some(error) => lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        ))),
        None => lines.push(Line::from(Span::styled(
            "  tab/up/down between fields   enter writes config.toml and credentials.toml   esc cancels",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    // The action line lives in the title as well as the body: at 80x24 the intro
    // prose pushes the body's last line off, and the key that submits the form must
    // never be the line that disappears (P-3).
    let block = Block::bordered()
        .title(" add an agent — enter writes it, esc cancels ")
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The one-field, masked prompt for a secret that goes into the credentials file.
fn draw_secret_prompt(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.secret_prompt else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let height = 8.min(screen.height);
    let popup = centered(screen, width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            y: popup.y,
            width: screen.width,
            height: popup.height,
        },
    );
    let count = prompt.value.chars().count();
    // The count, not the value: a person pasting a long token can see how much was
    // kept without the product ever printing the secret (P-2).
    let shown = if count == 0 {
        String::new()
    } else {
        format!(
            "{} ({count} characters, hidden)",
            "*".repeat(count.min(12))
        )
    };
    let mut lines: Vec<Line> = vec![
        Line::from(format!(
            "The {} is written to the credentials file and never shown.",
            prompt.target.describe()
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {shown}"),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        )),
        Line::from(""),
    ];
    match &prompt.error {
        Some(error) => lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        ))),
        None => lines.push(Line::from(Span::styled(
            "  enter writes it   esc cancels",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    let block = Block::bordered()
        .title(format!(" set the {} ", prompt.target.describe()))
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The "remove this agent?" confirmation, so a destructive key is never silent.
fn draw_remove_confirm(frame: &mut Frame, app: &App) {
    let Some(name) = &app.confirm_remove else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let height = 7.min(screen.height);
    let popup = centered(screen, width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            y: popup.y,
            width: screen.width,
            height: popup.height,
        },
    );
    let left = app
        .selected_paths()
        .map(|paths| paths.config_dir().display().to_string())
        .unwrap_or_else(|| "its files".to_owned());
    let has_files = app
        .selected_paths()
        .map(|paths| paths.config_dir().is_dir())
        .unwrap_or(false);
    // The confirmation's job is to say exactly what stays. A sentence that promises
    // files to a user who never wrote any is a small lie that makes them hunt for a
    // directory that does not exist.
    let question = if has_files {
        format!(
            "Remove `{name}` from the configuration? Its files under {left} are left on disk."
        )
    } else {
        format!("Remove `{name}` from the configuration? It has no files under {left} yet.")
    };
    let lines: Vec<Line> = vec![
        Line::from(question),
        Line::from(""),
        Line::from(Span::styled(
            "y remove it        n keep it",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let block = Block::bordered()
        .title(" remove an agent ")
        .border_style(Style::new().fg(Color::Red));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The no-`$EDITOR` confirmation, over the screen, before the editor takes over.
///
/// A status line would be erased the instant the UI steps aside for the editor, so the
/// program is named here and the user says when to go.
fn draw_editor_confirm(frame: &mut Frame, app: &App) {
    let Some(action) = &app.editor_confirm else {
        return;
    };
    let screen = overlay_area(frame.area());
    let program = crate::editor::editor();
    let width = (screen.width.saturating_sub(2)).min(74);
    let height = 7.min(screen.height);
    let popup = centered(screen, width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            y: popup.y,
            width: screen.width,
            height: popup.height,
        },
    );
    let mut lines: Vec<Line> = vec![
        Line::from(format!(
            "No `$EDITOR` is set, so {} opens in:",
            action.label
        )),
        Line::from(Span::styled(
            format!("  {program}"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
    ];
    if program.trim() == "vi" {
        lines.push(Line::from(
            "  In vi: press i to type, Esc to stop typing, then :wq and enter to save and quit, or :q! to leave without saving.",
        ));
    }
    lines.push(Line::from(Span::styled(
        "Set `$EDITOR` (for example, export EDITOR=nano) to use another editor.",
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "enter open it        esc cancel",
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    let block = Block::bordered()
        .title(" open an editor ")
        .border_style(Style::new().fg(Color::Yellow));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The key-reference overlay: every key, in groups, one line each.
fn draw_help(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    let width = (screen.width.saturating_sub(2)).min(80);
    // The help is a list, so it takes the list rule: a window it can scroll, and a
    // title that says the range. Every logical line is given room for two wrapped
    // rows, so a windowed line is never the line that clips (P-6).
    let capacity = (screen.height.saturating_sub(4) as usize / 2)
        .max(1)
        .min(HELP_LINES.len());
    let total = HELP_LINES.len();
    let max_first = total.saturating_sub(capacity);
    let first = app.help_offset.min(max_first);
    let shown = capacity.min(total - first);
    let height = (shown as u16 * 2 + 2).min(screen.height);
    let popup = centered(screen, width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            y: popup.y,
            width: screen.width,
            height: popup.height,
        },
    );
    let lines: Vec<Line> = HELP_LINES[first..first + shown]
        .iter()
        .map(|line| Line::from(*line))
        .collect();
    let title = if total > shown {
        format!(" keys {} ", list_title("", first, shown, total).trim())
    } else {
        " keys ".to_owned()
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// Every key the product binds, with one line of what it does.
const HELP_LINES: &[&str] = &[
    "Screens:   1-6 open a screen   tab / left / right move   esc back to Agents",
    "Moving:    up / down choose   enter open the chosen agent (or add one when none)",
    "An agent:  s start   x stop   p test its endpoint   m switch tui/systemd supervision",
    "           e edit its instruction   t edit its declared steps   K start a new skill",
    "           o open its newest log   T open its newest transcript   d remove it",
    "A new one: n or a open the add-agent form (also enter on an empty list)",
    "Forgejo:   c check the configured role   C check the administrator token",
    "           i settle the agent's identity   up/down choose a repository",
    "           g clone it   P open a pull request   U update the newest one",
    "Secrets:   k set the model key (Credentials)   f set the role token (Credentials)",
    "           v set the agent's own Forgejo token (Forgejo)   A set the administrator token",
    "Files:     r re-read config.toml and credentials.toml   q quit   ^C quit and stop runs",
    "Help:      ? or h or F1 show this list   esc or enter close it",
];

/// The first-run risk acknowledgment, over everything else.
fn draw_acknowledgment(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    // The modal sits over the body, above the footer, so the status line that says
    // what answers the screen stays visible while the modal is up. At least one cell
    // of margin on every side keeps it off the panel edge even on a narrow terminal.
    let available = Rect {
        x: screen.x,
        y: screen.y.saturating_add(3),
        width: screen.width,
        height: screen.height.saturating_sub(8),
    };
    let width = (available.width.saturating_sub(2)).min(78);
    // Size to the content, not to a fixed guess: the record path is a wrapped line,
    // and a modal that clips it ends the sentence without its object (W-7). The
    // available area is already inset from the panel, so it is the cap.
    let height = (risk::LINES.len() as u16 + 10).min(available.height);
    let popup = centered(available, width, height);

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
    // The modal owns every row it covers: clear the full terminal width for those
    // rows before drawing it, so the panel's border and wrapped notes cannot survive
    // beside it and collide with its own edges. The modal is drawn with at least one
    // cell of margin from the panel edge.
    let owned = Rect {
        x: 0,
        y: popup.y,
        width: available.width,
        height: popup.height,
    };
    frame.render_widget(Clear, owned);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The rows of a list that fit, and the first one to draw.
///
/// A list that silently drops rows — and lets the selection travel onto a row that is
/// not drawn — is the worst case for a person: they can start, stop or remove
/// something they cannot see. The window is chosen so the selected row is always
/// drawn, and the title says how many rows there are.
fn visible_window(total: usize, selected: usize, capacity: usize) -> (usize, usize) {
    if capacity == 0 || total == 0 {
        return (0, 0);
    }
    if total <= capacity {
        return (0, total);
    }
    let first = selected
        .saturating_add(1)
        .saturating_sub(capacity)
        .min(total - capacity);
    (first, capacity)
}

/// A panel title that names the rows on show when the list is longer than its window.
///
/// The product's own rule, applied to lists as it already is to the footer legend and
/// the window size: nothing is dropped **silently**.
fn list_title(base: &str, first: usize, shown: usize, total: usize) -> String {
    if total == 0 || total <= shown {
        return format!(" {base} ");
    }
    let mut extra = format!("{}–{} of {total}", first + 1, first + shown);
    let more_above = first > 0;
    let more_below = first + shown < total;
    match (more_above, more_below) {
        (true, true) => extra.push_str(" — ↑↓ more"),
        (false, true) => extra.push_str(" — ↓ more"),
        (true, false) => extra.push_str(" — ↑ more"),
        (false, false) => {}
    }
    format!(" {base} {extra} ")
}

/// The area an overlay may cover: the body, with the footer's status rows left free.
///
/// A modal that clears the status row silently hides the sentence that explains what
/// just happened — a paste that kept only its first line, or why a form was refused.
/// The footer is the product's voice; overlays do not cover it.
fn overlay_area(screen: Rect) -> Rect {
    Rect {
        x: screen.x,
        y: screen.y.saturating_add(3),
        width: screen.width,
        height: screen.height.saturating_sub(8),
    }
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
