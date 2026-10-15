//! Drawing the screens.
//!
//! [`draw`] is a plain function of [`App`] and a frame, so the same code paints the
//! real terminal and, in the tests, an in-memory backend: what the tests show is what
//! the terminal shows.
//!
//! Every screen that has nothing behind it says so, in the product's own words and
//! without a word from inside the product. No screen offers an action the program
//! cannot carry out: the state
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

/// The comfortable minimum width of a column of agent names.
///
/// The hub and the runs list used to fix the name column here and clip whatever was
/// longer with no marker, so two legal names that shared a prefix read identically
/// (P-2). The column now grows to the shortest width that keeps every configured name
/// apart, and never falls below this.
const NAME_COLUMN_MIN: usize = 14;

/// The room the hub's state column wants for the longest word it can show, and the
/// least it is given before it ellipsizes (E-7, P-5).
const STATE_COLUMN_MAX: usize = 19;
const STATE_COLUMN_MIN: usize = 12;

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
    // The footer grows to hold its status: a reason or a path is never the line that
    // clips. It may take the body's spare rows down to the body's own minimum, but no
    // further, so a long message is shown whole without hiding the screen behind it.
    let max_status = full.height.saturating_sub(9).max(3);
    // The status keeps its three rows even when it needs fewer, so the ordinary
    // layout is unchanged; it grows only for a status that does not fit in three.
    let status = status_rows(&app.status, full.width).clamp(3, max_status);
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(status + 2),
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
    } else if app.url_prompt.is_some() {
        draw_url_prompt(frame, app);
    } else if app.confirm_remove.is_some() {
        draw_remove_confirm(frame, app);
    } else if app.confirm_forget.is_some() {
        draw_forget_confirm(frame, app);
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
    // The number beside each name is the key that opens it, so the mapping the help
    // lists is on the bar itself (C-5); the name keeps its own word.
    let titles: Vec<Line> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(at, screen)| Line::from(format!("{} {}", at + 1, screen.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.screen.index())
        .divider(" ")
        // No side padding: the divider already separates the names, and the padding
        // was what pushed `6 Runs` past 72 columns, clipping it to `6 Ru` at the
        // advertised floor (A-4). Every number keeps its whole name here.
        .padding("", "")
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
    let notes_height = area.height.saturating_sub(4).min(10);
    let parts =
        Layout::vertical([Constraint::Min(4), Constraint::Length(notes_height)]).split(area);

    let capacity = parts[0].height.saturating_sub(3) as usize;
    let (first, shown) = visible_window(app.agents().len(), app.selected, capacity);
    // The name column is sized to the names themselves: a legal name is never clipped
    // into a neighbour without a marker (P-2), and the state column gets the room for
    // the longest state word (E-7, P-5).
    let [name_w, model_w, concurrency_w, supervision_w, state_w] = hub_column_widths(app, parts[0]);

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
                Cell::from(ellipsise(&agent.name, name_w as usize)),
                Cell::from(ellipsise(&agent.model, model_w as usize)),
                Cell::from(agent.concurrency.to_string()),
                Cell::from(app.supervisor.mode(&agent.name).as_str()),
                Cell::from(ellipsise(
                    &app.supervisor.state(&agent.name).cell_word(),
                    state_w as usize,
                )),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(name_w),
        Constraint::Length(model_w),
        Constraint::Length(concurrency_w),
        Constraint::Length(supervision_w),
        Constraint::Length(state_w),
    ];
    let table = Table::new(rows, widths).header(header).block(
        Block::bordered().title(list_title("agents", first, shown, app.agents().len())),
    );
    frame.render_widget(table, parts[0]);

    // A note area that runs out of rows declares its tail instead of dropping it
    // without a word — the same rule the panels follow in their titles (O-3) — and a
    // note area scrolled away from its top says so too (A-1).
    // F-1: while the acknowledgment modal owns the screen, the note area behind it is
    // not drawn at all. It used to be drawn with only its declaration suppressed, so
    // the modal could cover the head of a wrapped sentence and leave its tail
    // (`writes both files for you.`) as the first screen a zero-knowledge user sees.
    // The modal is the screen while it is up; the note area returns when it closes.
    let notes = if app.show_acknowledgment {
        Vec::new()
    } else {
        note_window(
            agent_notes(app),
            area.width as usize,
            parts[1].height as usize,
            app.panel_scroll,
            true,
        )
    };
    frame.render_widget(
        Paragraph::new(Text::from(notes)).wrap(Wrap { trim: true }),
        parts[1],
    );
}

/// The lines under the agent list.
fn agent_notes(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(error) = &app.config_error {
        // The recovery key belongs with the error, not only in the legend that a short
        // window clips (T-7): say how to try the file again after fixing it.
        lines.push(Line::from(Span::styled(
            format!("no agent list: {error} — fix the file, then press r to read it again"),
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
    // C-3: the acknowledgment record is a fact a second person needs before anything
    // else on this screen, so it is drawn near the top, where a short note area cannot
    // hide it below the fold. A first user who answered the overlay never thinks about
    // it again; the person who inherits the directory must be able to read what was
    // agreed, when and under which version.
    if !app.ack_confirmed() {
        lines.push(Line::from(Span::styled(
            "The risk of running an agent is not acknowledged: starting one is refused.",
            Style::new().fg(Color::Yellow),
        )));
    } else if let Some(record) = app.ack_state.record_line() {
        lines.push(Line::from(Span::styled(record, Style::new().fg(Color::DarkGray))));
    }
    let waiting = app.supervisor.waiting_count();
    let waiting_noun = if waiting == 1 { "start" } else { "starts" };
    lines.push(Line::from(format!(
        "Running: {} of {} allowed by the configuration; {waiting} {waiting_noun} waiting for a slot.",
        app.supervisor.running_count(),
        app.supervisor.max_concurrent(),
    )));
    // Why the selected agent's last run failed, on the screen the user looks at. A
    // start that never became a run is named as a start, not as a run (D-8).
    if let Some(name) = app.selected_name() {
        if let Some(reason) = app.last_run_failure(&name) {
            // Y-4: when a record the hub had is gone, the newest record left is not
            // silently promoted to "the last run". Say a record went missing, and
            // call the older failure what it is.
            let label = if app.forgotten_run.as_deref() == Some(name.as_str()) {
                format!("the newest run record left for `{name}` did not go well")
            } else {
                format!("the last run of `{name}` did not go well")
            };
            lines.push(Line::from(Span::styled(
                format!("{label}: {}", app.reason_with_history(&name, &reason)),
                Style::new().fg(Color::Red),
            )));
        }
        if app.forgotten_run.as_deref() == Some(name.as_str()) {
            lines.push(Line::from(Span::styled(
                format!(
                    "a run record the hub had for `{name}` is no longer there; what is shown is what the records left on disk say"
                ),
                Style::new().fg(Color::Yellow),
            )));
        }
        if let Some(reason) = app.last_start_failure(&name) {
            lines.push(Line::from(Span::styled(
                format!("the last start of `{name}` did not go well: {reason}"),
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
            breakable_path(&app.supervisor.unit_dir().display().to_string())
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
    lines
}

/// One agent's configuration, read from the configuration file and the supervisor.
fn agent_config_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("name", &agent.name));
            lines.push(field("model", &agent.model));
            lines.push(field("endpoint", &agent.endpoint));
            lines.push(field("max turns", &agent.max_turns.to_string()));
            // The instruction the run reads, resolved against the configuration
            // directory. The Instruction screen and the run's own plan show the same
            // file, so this screen cannot present a path the run ignores (P-3).
            let instruction = app
                .selected_paths()
                .map(|paths| paths.instruction().display().to_string())
                .unwrap_or_else(|| agent.instruction.display().to_string());
            lines.push(field("instruction", &instruction));
            lines.push(field("concurrency", &agent.concurrency.to_string()));
            lines.push(Line::from(""));
            lines.push(field(
                "run state",
                &app.supervisor.state(&agent.name).word_with_reason(),
            ));
            if let Some(reason) = app.last_failure(&agent.name) {
                lines.push(field(
                    "last failure",
                    &app.reason_with_history(&agent.name, &reason),
                ));
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
                // W-2: a workspace that already has history is not only a path. The
                // branch, the commit and its age, whether it is dirty and whose
                // identity its commits carry are read from the repository; what the
                // product cannot read (no repository, no git) is said plainly.
                if let Some(note) = app.workspace_note() {
                    lines.push(field("repository", note));
                }
            }
            if let Some(problem) = app.selected_paths_problem() {
                lines.push(Line::from(Span::styled(
                    problem,
                    Style::new().fg(Color::Red),
                )));
            }
            match app.latest_run_log(&agent.name) {
                Some(log) if log.is_file() => {
                    // L-3: the log is the file that grows with every run, so its size
                    // is shown beside its path rather than left to the filesystem.
                    let size = App::file_size_text(&log)
                        .unwrap_or_else(|| "size unknown".to_owned());
                    lines.push(field(
                        "newest log",
                        &format!("{} ({size})", log.display()),
                    ));
                }
                // Y-7: a path to a file that is no longer there is not a thing. Mark it
                // gone, rather than presenting it as if `o` would open it.
                Some(log) => lines.push(field(
                    "newest log",
                    &format!(
                        "{} — gone, the record has no log; press o to see what it says",
                        log.display()
                    ),
                )),
                // W32-3: `no run yet` is a claim about the agent, and a person who
                // copied the configuration to a second machine is not told that their
                // agent never ran. The run records are machine-local, so the sentence
                // names what this machine knows.
                None => lines.push(field(
                    "newest log",
                    "no run record on this machine yet — run records live under the state directory",
                )),
            }
            // L-3: the transcript grows too, and was named nowhere with a size.
            match app.latest_run_transcript(&agent.name) {
                Some(path) if path.is_file() => {
                    let size = App::file_size_text(&path)
                        .unwrap_or_else(|| "size unknown".to_owned());
                    lines.push(field(
                        "transcript",
                        &format!("{} ({size})", path.display()),
                    ));
                }
                Some(path) => lines.push(field(
                    "transcript",
                    &format!("{} — gone, the record has no transcript", path.display()),
                )),
                None => {}
            }
            if let Some(run_id) = app.supervisor.state(&agent.name).run_id() {
                lines.push(field("run", run_id));
            }
            // LIB-2: the field is always drawn. A test whose stamp survived a restart
            // is shown with its age; an agent with no test at all says so, so a person
            // never sees a fact silently missing where the map says it is.
            match app.endpoint_test_for(&agent.name) {
                // Y-5: a result about an endpoint or model that has since changed is
                // history; it is dated and marked, never shown as if it were now.
                Some(test) if test.is_current_for(agent) => {
                    // W32-2: a test belongs to a machine as well as a time. A record
                    // that names another machine is not this machine's result, and a
                    // record that names none says so — neither is drawn as if this
                    // machine had answered.
                    use crate::machine::Relation;
                    let base = test.describe();
                    let line = match test.machine_relation() {
                        Relation::Here => base,
                        Relation::Elsewhere => format!(
                            "{base} — but it was tested on another machine; press p to test it here"
                        ),
                        Relation::Unnamed => format!(
                            "{base} — the record does not name the machine it was tested on; press p to test it here"
                        ),
                        Relation::Undecidable => format!(
                            "{base} — this machine's own name cannot be read, so the product cannot say where it was tested; press p to test it here"
                        ),
                    };
                    lines.push(field("endpoint test", &line));
                }
                Some(test) => lines.push(field(
                    "endpoint test",
                    &format!(
                        "not current — the endpoint or model changed since it ran ({}); press p to test it again",
                        test.describe()
                    ),
                )),
                None => lines.push(field("endpoint test", "not tested yet — press p")),
            }
            match app.recorded_identity(&agent.name) {
                Some(identity) => lines.push(field("identity", &identity.line())),
                None => lines.push(field("identity", "not settled — press i")),
            }
            // A refusal from this session stays visible here; the success line is
            // already above, in the same shape every surface uses (F6).
            if let Some(report) = app.identity_report_for(&agent.name) {
                if report.result.is_err() {
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
    // A short window does not silently drop the fields below the fold: the panel says
    // so in its title, which is always drawn (D-7); the same rule is applied to every
    // sibling panel and note area (O-3).
    lines
}

/// Draw the Agent screen, windowed by the panel's own scroll key.
fn draw_agent_config(frame: &mut Frame, app: &App, area: Rect) {
    let lines = agent_config_lines(app);
    let above = app.panel_scroll > 0;
    let below = panel_overflows_from(&lines, area, app.panel_scroll);
    let (title, _) = fit_title(
        area.width as usize,
        "agent configuration",
        None,
        None,
        above,
        below,
    );
    draw_panel(frame, scrolled(&lines, app.panel_scroll), title, area);
}


/// An agent's instruction, its declared steps and its skills, as they are on disk.
fn instruction_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_agent() {
        None => lines.push(Line::from(no_agent_text(app))),
        Some(agent) => {
            lines.push(field("agent", &agent.name));
            match app.instruction_summary() {
                Some((path, exists, bytes)) => {
                    lines.push(field("instruction", &path.display().to_string()));
                    if !exists {
                        lines.push(field("  on disk", "not there yet — press e to write it"));
                    } else if app.instruction_is_only_placeholder() {
                        lines.push(field(
                            "  on disk",
                            "not there yet — press e to write it; the file holds only the guidance line",
                        ));
                    } else {
                        lines.push(field("  on disk", &format!("{bytes} bytes")));
                        // What a run sends, not only what is on disk (P-4): the two
                        // numbers and the reason for the difference. It is a line of
                        // its own, before the prose that can wait, so the sentence is
                        // not broken across rows after its own negation (C-7).
                        match app.instruction_payload() {
                            Some((on_disk, sent, dropped)) if dropped && on_disk != sent => {
                                lines.push(Line::from(format!(
                                    "a run sends {sent} of {on_disk} bytes — the guidance line is dropped"
                                )));
                            }
                            // No guidance line to drop: the only difference a run can
                            // make on its own is the document's trailing newline, and
                            // naming it for what it is (T-3, D-1) is the difference
                            // between a fact and a false reason.
                            Some((on_disk, sent, _)) if on_disk != sent => {
                                lines.push(Line::from(format!(
                                    "a run sends {sent} of {on_disk} bytes — the trailing newline is not sent"
                                )));
                            }
                            _ => {}
                        }
                    }
                    // An interrupted editor leaves its unsaved buffer beside the file;
                    // naming it is the difference between recovery and retyping.
                    if let Some(backup) = app.instruction_backup() {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "an earlier edit of this instruction was not saved; the editor kept it in {} — open that file to recover",
                                breakable_path(&backup.display().to_string())
                            ),
                            Style::new().fg(Color::Yellow),
                        )));
                    }
                    // What a run will send, said before the panel's lower prose: it is
                    // the row the page map requires at this state, and a short window
                    // must not be the reason a person never reads it (O-3).
                    lines.push(Line::from(Span::styled(
                        format!(
                            "A run sends this instruction, the conversation so far and the declared `shell` tool to {} ({}), runs the tool calls it asks for inside the workspace, and stops when the model finishes or after {} {}.",
                            agent.endpoint,
                            agent.model,
                            agent.max_turns,
                            if agent.max_turns == 1 { "turn" } else { "turns" },
                        ),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
                None => lines.push(field("instruction", "the name cannot be used")),
            }
            match app.steps_summary() {
                Some((path, exists, count)) => {
                    lines.push(field("steps", &path.display().to_string()));
                    lines.push(field(
                        "  declared",
                        &if exists {
                            let noun = if count == 1 { "step" } else { "steps" };
                            format!("{count} shell {noun}, run in the workspace")
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
    lines
}

/// Draw the Instruction screen, windowed by the panel's own scroll key.
fn draw_instructions(frame: &mut Frame, app: &App, area: Rect) {
    let lines = instruction_lines(app);
    let above = app.panel_scroll > 0;
    let below = panel_overflows_from(&lines, area, app.panel_scroll);
    let (title, _) = fit_title(
        area.width as usize,
        "instruction, steps and skills",
        None,
        None,
        above,
        below,
    );
    draw_panel(frame, scrolled(&lines, app.panel_scroll), title, area);
}

/// The Forgejo connection, the role check and the repository list.
fn draw_forgejo(frame: &mut Frame, app: &App, area: Rect) {
    let inner = area.width.saturating_sub(2) as usize;
    let content_height = area.height.saturating_sub(2) as usize;
    let (mut head, all_repos) = forgejo_head_and_repos(app);
    let (tail, legend) = forgejo_frame(app);
    let tail_rows = pushed_rows(&tail, inner) + pushed_rows(&legend, inner);

    let long_name = "Forgejo connection and role check";
    let short_name = "Forgejo";
    let total = all_repos.len();

    // The windowed form is what the title describes, and it is drawn whenever it fits
    // and the user has not scrolled it: that is how the key legend stays on the panel
    // at 80×24 (Y-8). When it does not fit, the same content is drawn whole and
    // scrolled with `[`/`]` instead, so the list and the legend are reachable rather
    // than only declared (A-3).
    let (_first, _shown, overflows, range) = forgejo_window(
        &head,
        total,
        app.repo_selected,
        inner,
        content_height,
        tail_rows,
    );
    if !overflows && app.panel_scroll == 0 {
        let (first_title, dropped) = fit_title(
            area.width as usize,
            long_name,
            Some(short_name),
            range.as_deref(),
            false,
            false,
        );
        let title = if dropped {
            // The title cannot hold both the range and the declaration: the
            // declaration wins, and the range moves to a line above the list, so
            // neither is lost (Y-1).
            if let Some(range) = range {
                head.push(note_marker(&range));
            }
            fit_title(
                area.width as usize,
                long_name,
                Some(short_name),
                None,
                false,
                false,
            )
            .0
        } else {
            first_title
        };
        // The window the title has just described is the one drawn: the range line
        // (when it moved to the body) is part of `head`, so it is counted here.
        let (first, shown, _, _) = forgejo_window(
            &head,
            total,
            app.repo_selected,
            inner,
            content_height,
            tail_rows,
        );
        let repo_rows: Vec<Line> = all_repos[first..first + shown].to_vec();
        let lines = assemble(head, repo_rows, tail, legend);
        draw_panel(frame, lines, title, area);
        return;
    }

    // The scrollable form: every repository is drawn, the range is a line above the
    // list, and the tail and legend end the panel. `[`/`]` (and the arrows, on this
    // screen the list keeps them) move the window over it.
    let lines = forgejo_scrolled_lines(app);
    let above = app.panel_scroll > 0;
    let below = panel_overflows_from(&lines, area, app.panel_scroll);
    let (title, _) = fit_title(
        area.width as usize,
        long_name,
        Some(short_name),
        None,
        above,
        below,
    );
    draw_panel(frame, scrolled(&lines, app.panel_scroll), title, area);
}

/// The Forgejo key legend, two compact rows.
///
/// Each phrase keeps its object: `v agent token` says what `v` sets, and the panel
/// clips whole rows, never half a sentence (Y-8). Keeping it to two rows is also what
/// leaves the repository list more than one row at 80×24.
fn forgejo_legend() -> Vec<Line<'static>> {
    [
        "u url   c role   C admin role   v agent token   A admin token   i identity",
        "up/down choose one   g clone   P pull request   U update the newest",
    ]
    .iter()
    .map(|line| {
        Line::from(Span::styled(
            (*line).to_owned(),
            Style::new().fg(Color::DarkGray),
        ))
    })
    .collect()
}

/// Put a panel's three parts together in reading order.
fn assemble(
    head: Vec<Line<'static>>,
    repo_rows: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    legend: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let mut lines = head;
    lines.extend(repo_rows);
    lines.extend(tail);
    lines.extend(legend);
    lines
}

/// The repository window, with the rows above it and the tail below it both reserved.
///
/// Returns the first row, the number of rows and whether the panel's content overflows
/// its window. Reserving the tail is what keeps the key legend on the panel instead of
/// letting the repository list push it off (Y-8).
fn forgejo_window(
    head: &[Line<'static>],
    total: usize,
    selected: usize,
    inner: usize,
    content_height: usize,
    tail_rows: usize,
) -> (usize, usize, bool, Option<String>) {
    let head_rows = pushed_rows(head, inner);
    let capacity = content_height.saturating_sub(head_rows + tail_rows).max(1);
    let (first, shown) = visible_window(total, selected, capacity);
    let overflows = head_rows + shown + tail_rows > content_height;
    let range = (total > shown).then(|| {
        list_title("repositories", first, shown, total)
            .trim()
            .to_owned()
    });
    (first, shown, overflows, range)
}

/// Every repository as a drawn row, for both the windowed and the scrolled form.
///
/// The selected row is marked in text as well as colour: a monochrome capture, or a
/// person who cannot tell the colours apart, still sees which repository `g` and `P`
/// will act on.
fn repository_rows(check: &crate::forgejo::RoleCheck, app: &App) -> Vec<Line<'static>> {
    check
        .repositories
        .iter()
        .enumerate()
        .map(|(at, repository)| {
            let style = if at == app.repo_selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(Span::styled(
                format!(
                    "{} {}",
                    if at == app.repo_selected { '>' } else { ' ' },
                    repository.line()
                ),
                style,
            ))
        })
        .collect()
}

/// The scrollable form of the Forgejo panel, every repository drawn.
///
/// The repository list is named above itself (the title may not be able to carry the
/// range and the overflow declaration at once — Y-1), and the tail and the key legend
/// close the panel exactly as they do in the windowed form, so `[`/`]` can always
/// reach them (A-3).
fn forgejo_scrolled_lines(app: &App) -> Vec<Line<'static>> {
    let (head, all_repos) = forgejo_head_and_repos(app);
    let (tail, legend) = forgejo_frame(app);
    let mut lines = head;
    if !all_repos.is_empty() {
        let total = all_repos.len();
        lines.push(note_marker(&format!("repositories 1–{total} of {total}")));
    }
    lines.extend(all_repos);
    lines.extend(tail);
    lines.extend(legend);
    lines
}

/// The rows above the repository list and every repository row, for the state on
/// show.
///
/// Both the windowed form and the scrollable form are built from these rows, so the
/// list a title describes and the list `[`/`]` reaches can never disagree (A-3).
fn forgejo_head_and_repos(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let mut head: Vec<Line> = Vec::new();
    match &app.config {
        None => head.push(Line::from(no_agent_text(app))),
        Some(config) => {
            head.push(field("url", &config.forgejo.url));
            head.push(field("role", config.forgejo.role.as_str()));
        }
    }
    let (member, admin) = app.forgejo_tokens_present();
    head.push(field(
        "role token",
        if member {
            "in the credentials file"
        } else {
            "absent"
        },
    ));
    head.push(field(
        "admin token",
        if admin {
            "configured (identities are managed)"
        } else {
            "absent (identities are supplied)"
        },
    ));
    if let Some(agent) = app.selected_agent() {
        // W32-5: the settled sentence and the last failure were two fields with no
        // word joining them, so a reader could not tell whether the identity was
        // usable now. The failure is appended to the identity line itself, and the
        // tail (`forgejo_frame`) no longer repeats it, so the panel has one verdict.
        let last_identity_failure = app
            .identity_report_for(&agent.name)
            .and_then(|report| report.result.err());
        let failure_clause = |failure: &crate::app::ForgejoFailure| {
            format!(
                " — the last check failed ({}): {}",
                failure.class, failure.message
            )
        };
        match app.recorded_identity(&agent.name) {
            Some(identity) => {
                // One shape on every surface, and it names the agent: the login, the
                // email and the mode of the account a clone or a pull request acts as
                // (F1, F6). A second person with several agents never has to guess
                // whose identity is on show.
                let mut line = format!(
                    "{} — clones and pull requests act as this account",
                    identity.line()
                );
                if let Some(failure) = &last_identity_failure {
                    line.push_str(&failure_clause(failure));
                }
                head.push(field("agent identity", &line));
            }
            None => {
                // What `i` will do, before it does it: with an administrator token the
                // tool creates and manages the account `gewu-<agent>`; without one, it
                // checks the token the user supplies with `v` (D-4). The field names
                // the agent, not only the account it would create (F1).
                let login = identity::login_for(&agent.name)
                    .unwrap_or_else(|_| format!("gewu-{}", agent.name));
                let (_, admin) = app.forgejo_tokens_present();
                // The field names the **agent** the identity would belong to, so a
                // second person with several agents never has to guess whose
                // selection the screen is showing (F1). The supplied path keeps the
                // page map's `v supplies the token` clause whole at 80×24.
                let mut sentence = if admin {
                    format!(
                        "not settled for `{}` — press i for `{login}`; the account is created and managed",
                        agent.name
                    )
                } else {
                    format!(
                        "not settled for `{}` — press i; v supplies the token",
                        agent.name
                    )
                };
                if let Some(failure) = &last_identity_failure {
                    sentence.push_str(&failure_clause(failure));
                }
                head.push(field("agent identity", &sentence));
            }
        }
    }
    // The definition of a role is what the screen owes *before* `c` is pressed; once
    // the check has run, the repositories are what the panel is for, and the list
    // gets the rows (T-12).
    if !matches!(app.role_check, Some(Ok(_))) {
        head.push(Line::from(Span::styled(
            ROLE_EXPLANATION,
            Style::new().fg(Color::DarkGray),
        )));
    }
    head.push(Line::from(""));

    let mut all_repos: Vec<Line> = Vec::new();
    match &app.role_check {
        None => head.push(placeholder(
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
            head.push(Line::from(Span::styled(
                format!(
                    "role check failed ({}) at {at}: {}",
                    failure.class, failure.message
                ),
                Style::new().fg(Color::Red),
            )));
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
            head.push(Line::from(summary));
            head.push(Line::from(""));
            if check.repositories.is_empty() {
                head.push(Line::from(
                    "the token can see no repository on this instance",
                ));
            } else {
                all_repos = repository_rows(check, app);
            }
        }
    }
    (head, all_repos)
}

/// The rows after the repository list: the identity report and a clone note, then the
/// key legend.
///
/// They are counted against the list's window, so the legend (which names `v`, the key
/// that supplies the agent's own token) is never itself the content that falls off the
/// panel at 80×24 (Y-8).
fn forgejo_frame(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let mut tail: Vec<Line> = Vec::new();
    // The report is drawn from the state that owns it: this session's result when
    // there is one, otherwise the identity recorded in the credentials file once the
    // role check has run, so a second person who restarts the product can still read
    // it (H-3).
    // W32-5: a **failure** is drawn on the identity line above, joined to the
    // settled sentence; drawing it here too would be the two-verdicts defect again.
    // The report (with its creation time) stays on the tail for a settled identity.
    if let Some(report) = app
        .selected_agent()
        .and_then(|agent| app.identity_report_for(&agent.name))
        .filter(|report| report.result.is_ok())
    {
        tail.push(Line::from(""));
        tail.push(Line::from(format!(
            "identity for `{}`: {}",
            report.agent,
            report.describe()
        )));
    }
    if let Some(note) = &app.forgejo_note {
        tail.push(Line::from(note.clone()));
    }
    tail.push(Line::from(""));
    (tail, forgejo_legend())
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
fn credentials_lines(app: &App) -> Vec<Line<'static>> {
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
    // The screen's own keys come first, before the prose: the page map requires this
    // row at every size ("`k` sets a model key ...") and at 80x24 the tail of the
    // prose used to be where they were dropped (O-3, P-M1).
    lines.push(Line::from(Span::styled(
        "k set the model key   f set the role token   A set the administrator token",
        Style::new().fg(Color::DarkGray),
    )));
    // The screen that shows a leftover secret is the screen that must offer its
    // delete: when the file holds a key for an agent no longer configured, `D` joins
    // the legend instead of living only on a help page (A-5).
    if !app.leftover_secret_agents().is_empty() {
        lines.push(Line::from(Span::styled(
            "D delete the leftover secrets of agents no longer configured",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    match &app.credentials_error {
        None => lines.push(Line::from(
            "The file is created with mode 0600 if it is absent, and held back to 0600 whenever the product re-reads the file; a file widened while the product is running is refused in the meantime.",
        )),
        Some(error) => lines.push(Line::from(Span::styled(
            format!("the credentials file is not ready: {error}"),
            Style::new().fg(Color::Red),
        ))),
    }
    // F-2: the configuration file is written 0600 too, and every directory the
    // product creates is 0700. The one exception is named, so the sentence is the
    // whole custody and an auditor can check it against the filesystem.
    lines.push(Line::from(
        "The configuration file is written with mode 0600 too; every directory the product makes inside its own configuration and state directories is held to 0700, and the one systemd unit is 0644 because it holds no secret — it names paths only.",
    ));
    lines.push(Line::from(
        "Its contents are never printed: nothing here is shown, logged or put on a command line.",
    ));
    // W32-6: the product never said it is one machine. The configuration directory,
    // the credentials file, the acknowledgment record and the endpoint-test record
    // belong to the machine they were made on, and a person who copies the directory
    // to a second machine must be told what travels with it and what stays behind.
    lines.push(Line::from(
        "This configuration, its credentials, the acknowledgment and the endpoint-test record belong to the machine they were made on; copying the configuration directory to another machine carries the Forgejo address, the secrets and the endpoint test with it, but a record that names another machine is labelled as such where it is shown. The run records live under the state directory and do not travel with the configuration.",
    ));
    lines.push(Line::from(Span::styled(
        "An agent runs as you, so it can read this file with your rights. The screen shows you where it is.",
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "The model key for an agent is read from this file at the point of a model call, and is redacted from every transcript, log and error the tool writes.",
        Style::new().fg(Color::DarkGray),
    )));
    lines
}

/// Draw the Credentials screen, windowed by the panel's own scroll key.
fn draw_credentials(frame: &mut Frame, app: &App, area: Rect) {
    let lines = credentials_lines(app);
    let above = app.panel_scroll > 0;
    let below = panel_overflows_from(&lines, area, app.panel_scroll);
    let (title, _) = fit_title(
        area.width as usize,
        "credentials file",
        None,
        None,
        above,
        below,
    );
    draw_panel(frame, scrolled(&lines, app.panel_scroll), title, area);
}

/// The recent runs, read from the records on disk.
fn draw_runs(frame: &mut Frame, app: &App, area: Rect) {
    // The same rule as the agent list: never silently drop a run, and never let the
    // selection land on a row that is not drawn. The note area takes the rows its own
    // content needs (the reason leads, and `o`/`T` must survive beside it), never
    // leaving the list with less than its border, its header and **three** runs: a
    // window of one row is a window a person cannot survey (O-3, P-M3, Y-3).
    let notes = run_notes(app);
    let inner = area.width.saturating_sub(2) as usize;
    let needed = pushed_rows(&notes, inner) as u16 + 2;
    // Border, header and three run rows belong to the list before the note area takes
    // anything.
    let list_min = 6;
    let notes_max = area.height.saturating_sub(list_min).max(4);
    let notes_height = needed.clamp(4, notes_max);
    let parts =
        Layout::vertical([Constraint::Min(4), Constraint::Length(notes_height)]).split(area);

    let capacity = parts[0].height.saturating_sub(3) as usize;
    let (first, shown) = visible_window(app.recent.len(), app.run_selected, capacity);
    // The same name column as the hub: a run's agent is a configured name, and it is
    // never clipped into a neighbour without a marker (P-2).
    const RUN_ID: usize = 22;
    const RUN_ID_MIN: usize = 10;
    const STARTED: usize = 20;
    // `succeeded, 0.0s` is fifteen columns. The outcome column keeps room for it at
    // the advertised floor, taking the room from the run-id column (which is already
    // shortened with a marker) rather than ending mid-value at the box edge (T-3).
    const OUTCOME_MIN: usize = 16;
    let inner = (area.width as usize).saturating_sub(2);
    let available = inner.saturating_sub(4);
    let names: Vec<&str> = app.recent.iter().map(|run| run.agent.as_str()).collect();
    let name_budget = available
        .saturating_sub(2 + RUN_ID + STARTED + OUTCOME_MIN)
        .max(NAME_COLUMN_MIN);
    let name_w = name_column_width(&names, name_budget);
    // The five columns sum to exactly `available`, so the table never overflows its
    // panel and ratatui never clips the last column without a marker.
    let run_id_w = RUN_ID
        .min(available.saturating_sub(2 + name_w + STARTED + OUTCOME_MIN))
        .max(RUN_ID_MIN.min(available));
    let outcome_w = available.saturating_sub(2 + name_w + run_id_w + STARTED);

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
                Cell::from(ellipsise(&run.agent, name_w)),
                Cell::from(ellipsise(&run.run_id, run_id_w)),
                Cell::from(run.started_text()),
                Cell::from(ellipsise(&run.describe(), outcome_w)),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(name_w as u16),
        Constraint::Length(run_id_w as u16),
        Constraint::Length(STARTED as u16),
        Constraint::Length(outcome_w as u16),
    ];
    let table = Table::new(rows, widths).header(header).block(
        Block::bordered().title(list_title("recent runs", first, shown, app.recent.len())),
    );
    frame.render_widget(table, parts[0]);

    let above = app.panel_scroll > 0;
    let below = panel_overflows_from(&notes, parts[1], app.panel_scroll);
    let (title, _) = fit_title(
        parts[1].width as usize,
        "run records",
        None,
        None,
        above,
        below,
    );
    draw_panel(frame, scrolled(&notes, app.panel_scroll), title, parts[1]);
}

/// The lines under the run list, for the run on show.
fn run_notes(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(format!(
        "Run records are read from {}.",
        breakable_path(&app.state_root().display().to_string())
    )));
    // L-2: records accumulate for ever, and nothing on any screen said where they live
    // as a footprint or what a person is expected to do about them. The count, the
    // room and the cleanup expectation are now on the screen that lists them; the
    // count includes records of agents the configuration no longer lists, so a
    // removed agent's history does not vanish (L-4).
    let footprint = &app.footprint;
    if footprint.records > 0 {
        let noun = if footprint.records == 1 {
            "record"
        } else {
            "records"
        };
        lines.push(Line::from(format!(
            "{} run {noun} on disk, {} of file bytes in total.",
            footprint.records,
            crate::app::filesize_text(footprint.bytes)
        )));
        if footprint.unconfigured_records > 0 {
            let noun = if footprint.unconfigured_records == 1 {
                "record belongs"
            } else {
                "records belong"
            };
            lines.push(Line::from(format!(
                "{} {noun} to an agent the configuration no longer lists; they stay on disk until they are deleted.",
                footprint.unconfigured_records
            )));
        }
        lines.push(Line::from(Span::styled(
            "To free their space, remove a run's directory under that path. The product only reads run records; it never deletes one by itself.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    // L-1, L-5: the list is capped at the newest fifty records. Say so at the point of
    // the cap, rather than presenting fifty as the whole history. Every total here is
    // named for what it counts: the configured agents' records are the list's own
    // population, `on disk` is every record, and nothing is left out of "not listed"
    // (F4, H-4).
    if app.recent_total > app.recent.len() {
        let listed = app.recent.len();
        let configured = app.recent_total;
        let unconfigured = app.footprint.unconfigured_records;
        let configured_older = configured - listed;
        let not_listed = configured_older + unconfigured;
        let text = if unconfigured == 0 {
            format!(
                "This list stops at the newest {listed} of the {configured} run records whose agents are configured: {configured_older} older records are not listed here, but they are still on disk."
            )
        } else {
            let older_noun = if configured_older == 1 {
                "older record"
            } else {
                "older records"
            };
            let unlisted_noun = if unconfigured == 1 {
                "record"
            } else {
                "records"
            };
            let all_noun = if not_listed == 1 { "record" } else { "records" };
            format!(
                "This list stops at the newest {listed} of the {configured} run records whose agents are configured ({} records are on disk in all): {configured_older} {older_noun} of configured agents and {unconfigured} {unlisted_noun} of agents the configuration no longer lists — {not_listed} {all_noun} in all — are not listed here, but they are still on disk.",
                app.footprint.records
            )
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::new().fg(Color::Yellow),
        )));
    }
    if app.recent.is_empty() {
        lines.push(Line::from(
            "No run of a configured agent has been recorded yet, so there is nothing to list.",
        ));
        // A person who came here to see whether anything ran needs the step that makes
        // a run, not only where the records would appear.
        lines.push(Line::from("Press s on the Agents screen (1) to start one."));
    } else if let Some(run) = app.selected_run() {
        // The reason leads, because it is what a person who returns to this screen
        // came for; the offers follow in the rows the note area grew for them, so a
        // wordy reason cannot hide `o` and `T` (O-3, P-M3).
        if let Some(reason) = run.reason.as_deref() {
            lines.push(Line::from(format!(
                "outcome: {} — {}",
                run.describe(),
                app.reason_with_history(&run.agent, reason)
            )));
        } else {
            lines.push(Line::from(format!("outcome: {}", run.describe())));
        }
        // The offers lead the reason's tail: `T` and `o` are the page map's must-offers
        // at 80×24, so they are on one line above the paths and the prose, and cannot
        // be the rows the note area drops (O-3, P-M3).
        lines.push(Line::from(
            "T opens the transcript on show   o opens the log on show",
        ));
        // `esc` is this product's way back everywhere, but not the pager's; name the
        // pager's own key on the screen, before the key is pressed (M-5).
        lines.push(Line::from(Span::styled(
            "Both open in your $PAGER; leave the pager with q. `esc` is not the pager's key: it can swallow the next key, so if the pager ignores it, press q again.",
            Style::new().fg(Color::DarkGray),
        )));
        // The paths are the detail: named here, below the offers, so a short note area
        // drops them rather than the offers (they stay reachable with `[`/`]`). Each
        // file's size is beside it, so the growth a Long-hauler watches is readable
        // without leaving the product (L-3).
        let transcript = run.dir.join(crate::transcript::TRANSCRIPT_FILE);
        let transcript_size = App::file_size_text(&transcript)
            .unwrap_or_else(|| "size unknown".to_owned());
        let log_size =
            App::file_size_text(&run.log).unwrap_or_else(|| "size unknown".to_owned());
        lines.push(Line::from(format!(
            "the transcript is at {} ({transcript_size}) and the log at {} ({log_size})",
            breakable_path(&transcript.display().to_string()),
            breakable_path(&run.log.display().to_string())
        )));
        lines.push(Line::from(Span::styled(
            "The outcome is decided by the process, not by what it wrote.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines
}

/// The widths of the hub's five data columns.
///
/// The name column is widened to the shortest width that keeps every configured name
/// distinct, up to the room the window has; the state column is given the room for its
/// longest word; the model column takes what is left. When the window is too narrow
/// for all of them, the state gives ground first, then the name falls back to its
/// comfortable minimum, so identity outlives the labels.
fn hub_column_widths(app: &App, area: Rect) -> [u16; 5] {
    const MARKER: usize = 2;
    const GAPS: usize = 5;
    const CONCURRENCY: usize = 11;
    const SUPERVISION: usize = 11;
    const MODEL_MIN: usize = 8;
    let inner = (area.width as usize).saturating_sub(2);
    let available = inner.saturating_sub(GAPS);
    let names: Vec<&str> = app.agents().iter().map(|agent| agent.name.as_str()).collect();
    // The name may use whatever is left once the other columns have their minimums.
    let name_budget = available
        .saturating_sub(MARKER + CONCURRENCY + SUPERVISION + STATE_COLUMN_MIN + MODEL_MIN)
        .max(NAME_COLUMN_MIN);
    let name = name_column_width(&names, name_budget);
    let state_wanted = app
        .agents()
        .iter()
        .map(|agent| app.supervisor.state(&agent.name).cell_word().chars().count())
        .max()
        .unwrap_or(0)
        .clamp(STATE_COLUMN_MIN, STATE_COLUMN_MAX);
    let state = state_wanted
        .min(available.saturating_sub(MARKER + name + CONCURRENCY + SUPERVISION + MODEL_MIN))
        .max(STATE_COLUMN_MIN.min(available));
    let model = available
        .saturating_sub(MARKER + name + CONCURRENCY + SUPERVISION + state)
        .max(MODEL_MIN.min(available));
    [
        name as u16,
        model as u16,
        CONCURRENCY as u16,
        SUPERVISION as u16,
        state as u16,
    ]
}

/// The width of a column of agent names.
///
/// A name is at most `MAX_AGENT_NAME_LEN` characters and the configuration refuses
/// duplicates, so whole names are always distinct. When the window cannot hold the
/// longest name, the smallest width that still tells every distinct name apart
/// (keeping both ends) is used; `cap` is the room the caller can give.
fn name_column_width(names: &[&str], cap: usize) -> usize {
    let cap = cap.max(NAME_COLUMN_MIN);
    let mut distinct: Vec<&str> = names.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    for width in NAME_COLUMN_MIN..=cap {
        let mut rendered: Vec<String> =
            distinct.iter().map(|name| ellipsise(name, width)).collect();
        rendered.sort();
        rendered.dedup();
        if rendered.len() == distinct.len() {
            return width;
        }
    }
    cap
}

/// Shorten text to `max` characters, marking the cut with an ellipsis in the middle.
///
/// A column that clips a run id mid-token (`run-1791985790094-000`) reads as a
/// different id rather than as a shortened one (M-7); an explicit ellipsis keeps the
/// head and the tail, and the whole id is still in the note area's paths.
fn ellipsise(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 6 {
        return text.to_owned();
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// The status line and the key hints.
///
/// The status gets two rows so a path or a reason is not clipped. The action
/// line leads with `? all keys`, so at 80 columns nothing is hidden *silently*;
/// the full list is one key away.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // The status takes every row the caller gave the footer but the legend's two, so a
    // long reason or path wraps instead of being cut; the legend gets its own row and
    // is built to fit it, tail and all.
    let parts = Layout::vertical([
        Constraint::Min(1),
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
    // The legend is the product's own key list. Under an overlay that owns the plain
    // keys, `q` is a letter in a field or a cancel, never `quit`, so the line is
    // replaced with the one thing that is true there (E-2). The read-only
    // acknowledgment review closes on **any** key, so its footer says so instead of
    // naming `esc` as if the two disagreed (V-2).
    let legend = if app.show_acknowledgment && app.ack_review {
        "any key closes this overlay; the record is unchanged"
    } else if app.overlay_owns_keys() {
        "esc closes this overlay"
    } else {
        "q quit   esc agents   tab or left/right screen   1-6 screen   up/down select   enter open   r re-read"
    };
    frame.render_widget(
        Paragraph::new(legend)
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

/// How many rows the status needs at one width, wrapping counted.
///
/// The footer asks this before it lays itself out, so a status that carries a path, a
/// mode or git's own multi-line error is given the rows to be read whole (T-2, T-7).
/// The count is the terminal's own rule: words are kept whole where they fit, and a
/// word longer than the row (a path) is broken across rows rather than clipped.
fn status_rows(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let rows: usize = text
        .lines()
        .map(|line| wrapped_rows(line, width))
        .sum();
    rows.max(1) as u16
}

/// The rows every line already pushed into a panel will occupy at `width`, wrapping
/// counted. A window that picks its first row from this number never starts below a
/// row that has wrapped into several, so the selected row is always drawn.
fn pushed_rows(lines: &[Line], width: usize) -> usize {
    lines
        .iter()
        .map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            wrapped_rows(&text, width)
        })
        .sum()
}

/// Whether a bordered panel's wrapped content needs more rows than it can draw.
///
/// The Agent panel was the first to answer this in its title (D-7); every sibling
/// panel asks the same question now, so a short window declares what is below the
/// fold rather than dropping it without a word (O-3).
fn panel_overflows(lines: &[Line], area: Rect) -> bool {
    pushed_rows(lines, area.width.saturating_sub(2) as usize)
        > area.height.saturating_sub(2) as usize
}

/// Whether a scrolled panel still has content below the fold.
///
/// The question is asked of the lines from `offset` on, not of the whole panel: the
/// sixth round computed it for the unscrolled state only, so a scrolled panel dropped
/// the declaration entirely (A-1).
fn panel_overflows_from(lines: &[Line<'static>], area: Rect, offset: usize) -> bool {
    panel_overflows(&scrolled(lines, offset), area)
}

/// The sentence a bordered panel puts in its title when its content does not fit.
///
/// It is the one thing that must never be the part the terminal clips: a user who
/// cannot see the sentence cannot know that anything is below the fold (Y-1).
const MORE_BELOW: &str = "more below; enlarge the window";

/// The shorter form of the same sentence, for a title with no room for it all.
const MORE_BELOW_SHORT: &str = "more below";

/// The marker a panel or note area carries when it is scrolled away from its own
/// top.
///
/// The sixth round's rule was only computed for the down direction, so a panel the
/// user (or a screen change) had scrolled hid its own heading in silence (A-1). A
/// panel whose first drawn row is not its first line says so, in the same place the
/// `more below` sentence goes.
const MORE_ABOVE: &str = "↑ more above";

/// The shorter form of the marker, for a title with no room for it all.
const MORE_ABOVE_SHORT: &str = "↑ above";

/// The first drawn row of a scrolled borderless note area.
const MORE_ABOVE_NOTE: &str = "↑ more above — press [ to go back";

/// The last drawn row of a scrolled borderless note area whose tail still does not
/// fit.
const MORE_BELOW_NOTE: &str = "… more below; enlarge the window";

/// What a title says about the content it cannot draw, in both directions.
///
/// `full` asks for the sentence as it is normally read; otherwise a compact form is
/// offered so a title that cannot hold the whole thing still carries the meaning
/// (the Y-1 rule: the declaration is the part that must survive).
fn declaration_text(above: bool, below: bool, full: bool) -> Option<String> {
    match (above, below, full) {
        (false, false, _) => None,
        (true, true, true) => Some(format!("{MORE_ABOVE} — {MORE_BELOW}")),
        (true, true, false) => Some(format!("{MORE_ABOVE_SHORT}; {MORE_BELOW_SHORT}")),
        (true, false, true) => Some(MORE_ABOVE.to_owned()),
        (true, false, false) => Some(MORE_ABOVE_SHORT.to_owned()),
        (false, true, true) => Some(MORE_BELOW.to_owned()),
        (false, true, false) => Some(MORE_BELOW_SHORT.to_owned()),
    }
}

/// Compose a bordered panel's title from its parts, in the order they are read.
fn compose_title(base: &str, extra: Option<&str>, declaration: Option<&str>) -> String {
    let mut text = format!(" {}", base.trim());
    if let Some(extra) = extra {
        text.push_str(" — ");
        text.push_str(extra.trim());
    }
    if let Some(declaration) = declaration {
        text.push_str(" — ");
        text.push_str(declaration);
    }
    text.push(' ');
    text
}

/// A bordered panel title that always fits the width it is drawn into.
///
/// The declaration is what tells a user that content is below the fold, so it is the
/// part that must survive; when a panel carries more than its name — the Forgejo
/// panel carries its repository range — and both cannot fit, the shortened screen name
/// is used, then the shortened declaration, and only then is the range left out (the
/// caller names it in the body instead). Returns the title and whether the range was
/// left out of it (Y-1).
fn fit_title(
    width: usize,
    base: &str,
    short: Option<&str>,
    extra: Option<&str>,
    above: bool,
    below: bool,
) -> (String, bool) {
    let room = width.saturating_sub(2).max(1);
    let declarations = [
        declaration_text(above, below, true),
        declaration_text(above, below, false),
    ];
    let names: Vec<&str> = match short {
        Some(short) => vec![base, short],
        None => vec![base],
    };
    // Prefer the full declaration and the full name, with the range; fall back one
    // step at a time, and drop the range only when nothing else can hold both.
    for declaration in &declarations {
        for name in &names {
            if let Some(extra) = extra {
                let candidate = compose_title(name, Some(extra), declaration.as_deref());
                if candidate.chars().count() <= room {
                    return (candidate, false);
                }
            }
        }
    }
    for declaration in &declarations {
        for name in &names {
            let candidate = compose_title(name, None, declaration.as_deref());
            if candidate.chars().count() <= room {
                return (candidate, extra.is_some());
            }
        }
    }
    (
        compose_title(base, None, declaration_text(above, below, false).as_deref()),
        extra.is_some(),
    )
}

/// Slice a panel's logical lines so its body can be scrolled with a key.
///
/// The offset counts logical lines, not terminal rows: a line that wraps still moves
/// as one. The last line can always be brought to the top, so the very end of a panel
/// is reachable however short the window is (Y-2).
fn scrolled(lines: &[Line<'static>], offset: usize) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let first = offset.min(lines.len() - 1);
    lines[first..].to_vec()
}

/// How many logical lines the panel or note area on show can scroll through.
///
/// This is the number of lines `[`/`]` (and, on a screen with no list, the arrows and
/// page keys) can move the window over. Every surface with content reports its own
/// line count — Forgejo included, where the list, the prose and the legend are one
/// scrollable panel below 80×24 (A-3): the count is the scrollable form of that panel
/// (`forgejo_scrolled_lines`), so the offset can reach the repository list and the key
/// legend however short the window is.
#[must_use]
pub fn panel_total_lines(app: &App) -> usize {
    match app.screen {
        Screen::AgentConfig => agent_config_lines(app).len(),
        Screen::Instructions => instruction_lines(app).len(),
        Screen::Credentials => credentials_lines(app).len(),
        Screen::Agents => agent_notes(app).len(),
        Screen::Runs => run_notes(app).len(),
        Screen::Forgejo => forgejo_scrolled_lines(app).len(),
    }
}

/// Draw one bordered panel, already windowed by the caller.
///
/// A bordered panel never draws half a wrapped line: ratatui would clip the last
/// wrapped row at the border, and a sentence that ends at a box edge reads as a
/// rendering bug (W-3). Whole logical lines are dropped from the bottom until what
/// is left fits; the title's `more below` declaration is what says the rest exists,
/// and `[`/`]` reaches it.
fn draw_panel(frame: &mut Frame, lines: Vec<Line<'static>>, title: String, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let height = area.height.saturating_sub(2) as usize;
    let lines = fit_panel_lines(lines, width, height);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Drop whole logical lines from the bottom until what is left fits the panel.
///
/// A bordered panel never draws half a wrapped line: ratatui would clip the last
/// wrapped row at the border, and a sentence that ends at a box edge reads as a
/// rendering bug (W-3). The panel title's `more below` declaration says the rest
/// exists, and `[`/`]` reaches it.
fn fit_panel_lines(
    mut lines: Vec<Line<'static>>,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    while !lines.is_empty() && pushed_rows(&lines, width) > height {
        lines.pop();
    }
    lines
}

/// Keep a note area's tail, replacing what does not fit with a line that says so.
///
/// The Agents and Runs note areas are plain paragraphs with no title to carry the
/// declaration, so the last row carries it instead: the tail is dropped from the
/// bottom and one row is reserved for the sentence, which is never itself clipped
/// (O-3).
fn declared_notes(
    mut lines: Vec<Line<'static>>,
    width: usize,
    height: usize,
    declare: bool,
) -> Vec<Line<'static>> {
    if !declare {
        return lines;
    }
    if height == 0 || pushed_rows(&lines, width) <= height {
        return lines;
    }
    while !lines.is_empty() && pushed_rows(&lines, width) + 1 > height {
        lines.pop();
    }
    lines.push(note_marker(MORE_BELOW_NOTE));
    lines
}

/// A dim sentence used for a note area's own edge markers.
fn note_marker(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::new().fg(Color::DarkGray),
    ))
}

/// Window a borderless note area, saying which way its content continues.
///
/// At the top the rule is the sixth round's: the tail is dropped and the last row says
/// `more below`. Scrolled away from the top, the first drawn row says there is content
/// above (A-1) — and the tail still declares itself if it does not fit either. This is
/// the note-area form of the same both-direction rule the bordered panels carry in
/// their titles.
fn note_window(
    lines: Vec<Line<'static>>,
    width: usize,
    height: usize,
    offset: usize,
    declare: bool,
) -> Vec<Line<'static>> {
    if !declare {
        return lines;
    }
    if offset == 0 || height == 0 {
        return declared_notes(lines, width, height, declare);
    }
    let rest = scrolled(&lines, offset);
    let mut out = vec![note_marker(MORE_ABOVE_NOTE)];
    let room = height.saturating_sub(1);
    let mut rest = rest;
    if pushed_rows(&rest, width) > room {
        while !rest.is_empty() && pushed_rows(&rest, width) + 1 > room {
            rest.pop();
        }
        out.extend(rest);
        out.push(note_marker(MORE_BELOW_NOTE));
    } else {
        out.extend(rest);
    }
    out
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
        // A `+` when the newest fifty records all finished since this UI started and
        // older records are on disk: the count may be larger than it can see (L-5).
        let plus = if app.finished_count_is_capped() { "+" } else { "" };
        format!("{base}· {finished}{plus} finished   ")
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
    let height = (NewAgentForm::FIELDS.len() as u16 + 12).min(screen.height);
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
            "Name an agent and point it at a model. The model endpoint is the full address a run POSTs the request to, usually ending in /chat/completions; the model key and the Forgejo token are written to the credentials file and are never shown.",
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
    // Where the field block starts, so the window can keep the cursor's own row drawn.
    let fields_at = lines.len();
    for (at, (label, secret)) in NewAgentForm::FIELDS.iter().enumerate() {
        let value = &form.fields[at];
        // A masked value says how much it holds: the mask alone is the same block of
        // stars whether a paste landed once or twice (P-2).
        let shown = if *secret {
            let count = value.chars().count();
            if count == 0 {
                String::new()
            } else {
                format!("{} ({})", "*".repeat(count.min(12)), hidden_count(count))
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
    // A short window used to draw a `>` on a row it did not draw: the cursor walked
    // onto the two Forgejo fields invisibly (M-1). The body is windowed so the
    // cursor's own field is always among the drawn rows, and the title declares that
    // the form is longer than the window.
    let inner = popup.width.saturating_sub(2) as usize;
    let content_height = popup.height.saturating_sub(2) as usize;
    let cursor_line = fields_at + form.cursor;
    let cursor_rows = pushed_rows(&lines[cursor_line..=cursor_line], inner);
    let mut first = 0usize;
    while first < cursor_line
        && pushed_rows(&lines[first..cursor_line], inner) + cursor_rows > content_height
    {
        first += 1;
    }
    let above = first > 0;
    let below = pushed_rows(&lines[first..], inner) > content_height;
    // The action line lives in the title as well as the body: at 80x24 the intro
    // prose pushes the body's last line off, and the key that submits the form must
    // never be the line that disappears (P-3). LIB-1: the declaration is the same one
    // every other bordered surface uses, fitted to the width, and it names the
    // direction the window has moved in (above and/or below) instead of the one-off
    // phrase this form used to carry.
    let (title, _) = fit_title(
        popup.width as usize,
        "add an agent — enter writes it, esc cancels",
        None,
        None,
        above,
        below,
    );
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines[first..].to_vec()))
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
        format!("{} ({})", "*".repeat(count.min(12)), hidden_count(count))
    };
    let mut lines: Vec<Line> = vec![
        Line::from(format!(
            "The {} is written to the credentials file and never shown.",
            prompt.subject()
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
        .title(format!(" set the {} ", prompt.subject()))
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The one-field prompt for the configured Forgejo address (W32-1).
///
/// The address is not a secret, so it is drawn as itself — a masked address could not
/// be corrected. The prompt says what pressing enter changes: the one global address
/// every agent on this machine follows.
fn draw_url_prompt(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.url_prompt else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let height = 9.min(screen.height);
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
            "The address is the Forgejo every agent on this machine uses; this machine is the only one it names.",
        ),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {}", prompt.value),
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
            "  enter writes it for every agent   esc cancels",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    let block = Block::bordered()
        .title(" set the Forgejo instance address ")
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// How much a masked field holds, with the count in the right number.
fn hidden_count(count: usize) -> String {
    if count == 1 {
        "1 character, hidden".to_owned()
    } else {
        format!("{count} characters, hidden")
    }
}

/// The "remove this agent?" confirmation, so a destructive key is never silent.
fn draw_remove_confirm(frame: &mut Frame, app: &App) {
    let Some(name) = &app.confirm_remove else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let config_dir = app
        .selected_paths()
        .map(|paths| breakable_path(&paths.config_dir().display().to_string()))
        .unwrap_or_else(|| "its files".to_owned());
    let state_dir = breakable_path(&app.state_root().join(name).display().to_string());
    let config_exists = app
        .selected_paths()
        .map(|paths| paths.config_dir().is_dir())
        .unwrap_or(false);
    let state_exists = app.state_root().join(name).is_dir();
    // The confirmation's job is to say exactly what stays, and where. A sentence that
    // promises files to a user who never wrote any is a small lie that makes them hunt
    // for a directory that does not exist; one that names only the agent directory
    // leaves the run records and the secret unaccounted for (Y-6).
    let question = if config_exists || state_exists {
        format!(
            "Remove `{name}` from the configuration? Its files under {config_dir} and its run records under {state_dir} are left on disk."
        )
    } else {
        format!(
            "Remove `{name}` from the configuration? It has no files under {config_dir} yet, and no run records."
        )
    };
    let lines: Vec<Line> = vec![
        Line::from(question),
        Line::from(format!(
            "Its secret in {} is left too, marked on the Credentials screen; press D there to delete a secret whose agent is gone.",
            breakable_path(&app.credentials_path.display().to_string())
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y or enter remove it        n, esc or q keep it",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let rows = pushed_rows(&lines, width.saturating_sub(2) as usize) as u16;
    let height = (rows + 2).min(screen.height).max(6);
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

/// The "delete the secrets a removed agent left?" confirmation.
///
/// A removal leaves the agent's model key and its own Forgejo token in the credentials
/// file; until now nothing in the product could delete them (Y-6). The box names the
/// agents, never a value.
fn draw_forget_confirm(frame: &mut Frame, app: &App) {
    let Some(agents) = &app.confirm_forget else {
        return;
    };
    let screen = overlay_area(frame.area());
    let width = (screen.width.saturating_sub(2)).min(76);
    let names = agents
        .iter()
        .map(|agent| format!("`{agent}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let lines: Vec<Line> = vec![
        Line::from(format!(
            "Delete the leftover secrets for {names} from {}?",
            breakable_path(&app.credentials_path.display().to_string())
        )),
        Line::from("Their agents are not configured any more. The values are never shown."),
        Line::from(""),
        Line::from(Span::styled(
            "y or enter delete them        n, esc or q keep them",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let rows = pushed_rows(&lines, width.saturating_sub(2) as usize) as u16;
    let height = (rows + 2).min(screen.height).max(6);
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
    let block = Block::bordered()
        .title(" delete leftover secrets ")
        .border_style(Style::new().fg(Color::Red));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// An editor named as an example: never the one already chosen.
///
/// The unset hand-off names `nano` or `vi` as the program about to open; an example
/// that named the same one would make `to use another editor` impossible to follow
/// (V-3). The example is always the other of the two, so it changes something.
fn example_editor(current: &str) -> &'static str {
    if current.trim() == "nano" {
        "vi"
    } else {
        "nano"
    }
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
    let inner_width = width.saturating_sub(2) as usize;
    // The verb agrees with the label: `the declared steps … open`, `the instruction
    // … opens`. One fixed wording cannot be true of both (F6).
    let verb = if action.target.map(|target| target.is_plural()).unwrap_or(false) {
        "open"
    } else {
        "opens"
    };
    // N-1: the program is named **before** it takes the screen in both cases. The
    // set case used to be named only after the hand-off returned, because its status
    // line was erased as the UI stepped aside; the same confirmation the unset case
    // uses names it where a person can read it, and gives a way to back out.
    let mut body: Vec<Line> = vec![
        Line::from(format!(
            "{}, so {} {verb} in:",
            if app.editor_configured {
                "`$EDITOR` is set"
            } else {
                "No `$EDITOR` is set"
            },
            action.label
        )),
        Line::from(Span::styled(
            format!("  {program}"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
    ];
    if program.trim() == "vi" {
        body.push(Line::from(
            "  In vi: press i to type, Esc to stop typing, then :wq and enter to save and quit, or :q! to leave without saving.",
        ));
    } else if program.trim() == "nano" {
        // The overlay names the program; it must also say how to leave it, or a person
        // who presses `e` is stranded in an editor whose screen says esc does nothing
        // (M-4). The wording must be true in **both** cases: with nothing changed,
        // `^X` leaves at once and never asks, so a sentence that promises a prompt and
        // a `N` sends the person's `N` to the product, where it opens the add-agent
        // form (F5). `if it asks` is the part that is true either way.
        body.push(Line::from(
            "  In nano: ^X leaves; if it asks, Y and enter saves and N does not.",
        ));
    }
    if !app.editor_configured {
        // V-3: the example must not name the program already chosen — following it
        // would change nothing, and the sentence promises `another editor`. The
        // alternative is named, so the example always differs from the current one.
        body.push(Line::from(Span::styled(
            format!(
                "Set `$EDITOR` (for example, export EDITOR={}) to use another editor.",
                example_editor(&program)
            ),
            Style::new().fg(Color::DarkGray),
        )));
    }
    let answer: Vec<Line> = vec![Line::from(Span::styled(
        "enter open it        esc cancel",
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    // Size the box to what it holds, and never let it grow past the screen it is
    // drawn on. When a narrow floor forces a choice, whole body lines go from the
    // bottom and the action line stays; the title declares what was left out, so the
    // overlay follows the same rule as every other bordered surface (F-1's class).
    let wanted = pushed_rows(&body, inner_width) + pushed_rows(&answer, inner_width);
    let height = (wanted as u16 + 2).min(screen.height);
    let inner_height = height.saturating_sub(2) as usize;
    let mut dropped = false;
    while !body.is_empty()
        && pushed_rows(&body, inner_width) + pushed_rows(&answer, inner_width) > inner_height
    {
        body.pop();
        dropped = true;
    }
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
    let title = if dropped {
        fit_title(width as usize, "open an editor", None, None, false, true).0
    } else {
        " open an editor ".to_owned()
    };
    let mut lines = body;
    lines.extend(answer);
    let block = Block::bordered()
        .title(title)
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
    // The overlay may cover the body, but not the footer: a key it cannot use says so
    // on the status line, and that sentence must be readable while the overlay is up
    // (C-4). This is the same area the other overlays take.
    let screen = overlay_area(frame.area());
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
    "Panel:     [ / ] scroll the panel or note area; on Agent, Instruction and Credentials",
    "           the arrows and PageUp/PageDown move it too, so its last line is reachable",
    "Forgejo:   c check the configured role   C check the administrator token",
    "           i settle the agent's identity   up/down choose a repository",
    "           g clone it   P open a pull request   U update the newest one",
    "Secrets:   k set the model key (Credentials)   f set the role token (Credentials)",
    "           v set the agent's own Forgejo token (Forgejo)   A set the administrator token",
    "           D delete the leftover secrets of agents no longer configured (Credentials)",
    "Risk:      R read the acknowledged terms again; the record is not changed",
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

    let mut prose: Vec<Line> = Vec::new();
    for text in risk::LINES {
        prose.push(Line::from(vec![Span::raw("  * "), Span::raw(*text)]));
    }
    prose.push(Line::from(""));
    let mut answer: Vec<Line> = Vec::new();
    if app.ack_review {
        // Read-only: the record exists and a second person is reading it. Nothing
        // here writes, so nothing here may look like it does (C-3).
        answer.push(Line::from(Span::styled(
            "  any key closes this; the record is unchanged",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(record) = app.ack_state.record_line() {
            answer.push(Line::from(Span::styled(
                format!("  {record}"),
                Style::new().fg(Color::DarkGray),
            )));
        }
    } else {
        answer.push(Line::from(Span::styled(
            "  y — I understand, record it once        n — not now",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        answer.push(Line::from(
            "  Until it is recorded, starting an agent stays refused.",
        ));
        answer.push(Line::from(format!(
            "  The record will be written to {}",
            breakable_path(&app.ack_path.display().to_string())
        )));
    }

    // F-1: the modal is a bordered panel, so it obeys the same rule as every other
    // one — whole logical lines, never half a wrapped line. The answer line is the
    // modal's own key, so it is the last row to go: the prose is dropped from the
    // bottom first, then the sentences that follow the answer, and the title says
    // when anything was dropped (`more below; enlarge the window`). A person at the
    // floor therefore never reads a sentence that stops mid-thought in silence.
    let inner_width = popup.width.saturating_sub(2) as usize;
    let inner_height = popup.height.saturating_sub(2) as usize;
    let mut dropped = false;
    while answer.len() > 1 && pushed_rows(&answer, inner_width) > inner_height {
        answer.pop();
        dropped = true;
    }
    while !prose.is_empty()
        && pushed_rows(&prose, inner_width) + pushed_rows(&answer, inner_width) > inner_height
    {
        prose.pop();
        dropped = true;
    }
    let mut lines = prose;
    lines.extend(answer);

    let title = if dropped {
        // The declaration is the part that must survive, so it is fitted to the box
        // before it is drawn (Y-1).
        fit_title(
            popup.width as usize,
            risk::TITLE,
            None,
            None,
            false,
            true,
        )
        .0
    } else {
        format!(" {} ", risk::TITLE)
    };
    let block = Block::bordered()
        .title(title)
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
///
/// A path among the values gets break opportunities at its `/` separators, so a panel
/// too narrow for the whole path breaks between directories instead of splitting a
/// file name in half (H-5).
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<LABEL_WIDTH$}"),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw(breakable_path(value)),
    ])
}

/// A string whose `/` separators may break when a panel is too narrow.
///
/// Ratatui treats `U+200B` as a break opportunity and gives it no width, so the text
/// reads whole when it fits and, when it does not, breaks at a separator — never
/// inside a file name (H-5).
fn breakable_path(text: &str) -> String {
    text.replace('/', "/\u{200b}")
}

/// A line that says plainly that something is not there yet.
///
/// The sentence carries its own meaning; it never opens with a word from inside the
/// product (D-5).
fn placeholder(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("{text}."),
        Style::new().fg(Color::Yellow),
    ))
}

/// Why there is no agent to show.
fn no_agent_text(app: &App) -> String {
    match &app.config_error {
        Some(error) => format!(
            "no agent to show: {error} — fix the file, then press r to read it again"
        ),
        None => "no agent to show: the configuration lists none".to_owned(),
    }
}

/// The credentials file's mode, as read back from disk *now*.
///
/// The cached mode is what a file prepared when the app started had. A user who
/// widens the file by hand must see the mode it actually has, not a remembered
/// `0600` beside the refusal that says it is 0644 (D-2).
fn mode_text(app: &App) -> String {
    match crate::mode::mode_of(&app.credentials_path) {
        Ok(mode) => crate::mode::render(mode),
        Err(_) => "not read".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(count: usize) -> Vec<Line<'static>> {
        (0..count)
            .map(|at| Line::from(format!("line {at}")))
            .collect()
    }

    #[test]
    fn a_bordered_panel_never_draws_half_a_wrapped_line() {
        // W-3: at 80×24 the identity report was the panel's last row, wrapped and cut
        // at the border mid-sentence. A logical line that does not fit whole is not
        // drawn at all; the title's `more below` says the rest exists.
        let long = vec![Line::from("short"), Line::from("x".repeat(30))];
        // 1 + 3 wrapped rows fit exactly at height 4: both logical lines are kept.
        assert_eq!(fit_panel_lines(long.clone(), 10, 4).len(), 2);
        // One row fewer: the wrapped line cannot be drawn whole, so it is dropped.
        assert_eq!(
            fit_panel_lines(long, 10, 3).len(),
            1,
            "a partial wrap must not be drawn"
        );
        // The helper never invents rows.
        assert_eq!(fit_panel_lines(lines(5), 20, 100).len(), 5);
    }

    #[test]
    fn a_note_area_can_be_drawn_without_its_overflow_declaration() {
        // W-5: under the first-run modal the note area declared `more below` at a row
        // the modal hid. With the declaration suppressed, the content is drawn as it
        // is and no sentence points at hidden rows; everywhere else the rule holds.
        let over = lines(30);
        let declared = note_window(over.clone(), 20, 4, 0, true);
        assert_eq!(
            declared.last().map(|line| line.spans[0].content.as_ref()),
            Some(MORE_BELOW_NOTE)
        );
        let suppressed = note_window(over, 20, 4, 0, false);
        assert!(
            suppressed
                .iter()
                .all(|line| line.spans[0].content.as_ref() != MORE_BELOW_NOTE),
            "the declaration was drawn under the modal"
        );
        assert_eq!(suppressed.len(), 30, "the content itself is unchanged");
    }

    #[test]
    fn the_editor_example_never_names_the_editor_already_in_use() {
        // V-3: the unset hand-off named `nano` as the example while choosing `nano`
        // itself, so following the example changed nothing. The example is always the
        // other of the two fallbacks.
        assert_eq!(example_editor("nano"), "vi");
        assert_eq!(example_editor("vi"), "nano");
        assert_ne!(example_editor("nano"), "nano");
        assert_ne!(example_editor("vi"), "vi");
    }
}
