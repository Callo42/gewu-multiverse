//! Drawing the board.
//!
//! Everything is painted on the product's own background, cell by cell, so a
//! terminal's theme cannot show through. Titles and statuses sit on a border between
//! the half-line caps `╴` and `╶`, so every arm of every box-drawing cell has an
//! opposite arm in its neighbour. [`BoardLayout`] records the cards and the cells
//! above them so the render tests can check the picture and the test guard.

use crate::board::status;
use crate::board::text as text_util;
use crate::board::theme;
use crate::board::{Board, Screen};
use crate::engine_api::{
    AgentView, EndpointTest, ForgejoState, ForgejoView, Provisioning, SessionEnd, SessionState,
};
use crate::harness::Program;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// One card on the home screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSlot {
    /// Index into `BoardView.agents`.
    pub agent: usize,
    /// The rows of the card, including its border.
    pub rect: Rect,
}

/// How the home screen draws the agents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoardForm {
    /// A boxed card for each agent: two rows at the floor, three or more above it.
    #[default]
    Cards,
    /// One text line per agent inside one wired box, used when the cards cannot
    /// all fit (B1/B2). The line carries the mark, the name, the login and the run
    /// count; the state word follows when the column has room for it whole.
    List,
}

/// The rectangles and junctions drawn by the last home frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardLayout {
    /// Cards (or list lines) per row.
    pub columns: usize,
    /// The first agent index on screen.
    pub first_visible: usize,
    /// Every bordered card drawn (empty in the list form).
    pub cards: Vec<CardSlot>,
    /// Every one-line agent row drawn in the list form (empty in the card form).
    pub listed: Vec<CardSlot>,
    /// Which complete form the agents took.
    pub form: BoardForm,
    /// The Forgejo box.
    pub forgejo: Option<Rect>,
    /// The cell above each card's top centre where its wire arrives.
    pub wire_cells: Vec<(u16, u16)>,
    /// Whether cards are above or below the window.
    pub scrolls: bool,
    /// Rows the footer occupies, exactly matching the lines the footer draws:
    /// the two key lines plus one each for the status sentence and the event
    /// (B3), or the quit warning's wrapped lines plus its notice (review-2).
    pub footer_rows: u16,
}

/// The four arms of a box-drawing cell.
const L: u8 = 1;
const R: u8 = 2;
const U: u8 = 4;
const D: u8 = 8;

/// The character for a set of arms, if the set is one the board draws.
fn arms_char(arms: u8) -> Option<char> {
    Some(match arms {
        a if a == L | R => '─',
        a if a == U | D => '│',
        a if a == R | D => '╭',
        a if a == L | D => '╮',
        a if a == R | U => '╰',
        a if a == L | U => '╯',
        a if a == U | D | R => '├',
        a if a == U | D | L => '┤',
        a if a == L | R | D => '┬',
        a if a == L | R | U => '┴',
        a if a == L | R | U | D => '┼',
        a if a == L => '╴',
        a if a == R => '╶',
        a if a == U => '╵',
        a if a == D => '╷',
        _ => return None,
    })
}

/// Draw one cell of a connection.
fn put_arms(buf: &mut Buffer, clip: Rect, x: u16, y: u16, arms: u8, style: Style) {
    if x < clip.x || x >= clip.right() || y < clip.y || y >= clip.bottom() {
        return;
    }
    if let Some(ch) = arms_char(arms) {
        text_util::put(buf, clip, x, y, &ch.to_string(), style);
    }
}

fn inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Start tall content near the top instead of centring it into a mostly empty
/// frame. A screen that nearly fills its frame stays centred; a large window uses
/// its rows for the picture, and the residual space falls below the content.
fn content_origin(top: u16, height: u16, block: u16) -> u16 {
    let spare = height.saturating_sub(block);
    if spare >= 4 {
        top.saturating_add(1)
    } else {
        top.saturating_add(spare / 2)
    }
}

/// Fill a rectangle with the product background and draw a plain border.
fn draw_plain_box(buf: &mut Buffer, rect: Rect, style: Style) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    text_util::fill(buf, rect, style);
    let right = rect.right() - 1;
    let bottom = rect.bottom() - 1;
    for x in rect.x + 1..right {
        put_arms(buf, rect, x, rect.y, L | R, style);
        put_arms(buf, rect, x, bottom, L | R, style);
    }
    for y in rect.y + 1..bottom {
        put_arms(buf, rect, rect.x, y, U | D, style);
        put_arms(buf, rect, right, y, U | D, style);
    }
    put_arms(buf, rect, rect.x, rect.y, R | D, style);
    put_arms(buf, rect, right, rect.y, L | D, style);
    put_arms(buf, rect, rect.x, bottom, R | U, style);
    put_arms(buf, rect, right, bottom, L | U, style);
}

/// Draw the outer frame and its title, with caps closing the line around the title.
fn draw_outer(buf: &mut Buffer, area: Rect, title: &str) {
    text_util::fill(buf, area, theme::base());
    draw_plain_box(buf, area, theme::fg(theme::WIRE));
    if !title.is_empty() && area.width > 8 {
        border_label(
            buf,
            area,
            area.x + 2,
            area.y,
            title,
            theme::bold(theme::ACCENT),
            area.width.saturating_sub(6) as usize,
        );
    }
}

/// Put a label on a border line, between two half-line caps.
///
/// The caps are what makes the label safe for the connectivity guard: the border
/// cells before and after the label meet a cap's one arm, and the caps have no arm
/// pointing into the text.
fn border_label(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
    max_width: usize,
) {
    border_label_with(buf, clip, x, y, text, style, max_width, false);
}

/// A border label cut on whole words, for a card's status line.
///
/// A card's bottom border carries a sentence, and a character cut turns
/// `HTTP 500` into `HTTP 5…`. The caller puts the fact that must survive first
/// (the HTTP status), so a word cut never removes it (F-14).
fn border_label_words(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
    max_width: usize,
) {
    border_label_with(buf, clip, x, y, text, style, max_width, true);
}

/// The tight card label: no padding around the text, so a three-row card at
/// 80 columns can hold both `✗ HTTP 500` and `stopped` (F-14). It is still a
/// whole-word cut, and the caps keep it safe for the connectivity guard.
fn border_label_tight(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
    content_room: usize,
) {
    let text = text_util::ellipsize_words(text, content_room);
    let width = text_util::width(&text) as u16;
    if width == 0 || x < clip.x || x + width + 2 > clip.right() {
        return;
    }
    put_arms(buf, clip, x, y, L, style);
    text_util::put(buf, clip, x + 1, y, &text, style);
    put_arms(buf, clip, x + width + 1, y, R, style);
}

#[allow(clippy::too_many_arguments)]
fn border_label_with(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
    max_width: usize,
    whole_words: bool,
) {
    let room = max_width.saturating_sub(4);
    let cut = if whole_words {
        text_util::ellipsize_words(text, room)
    } else {
        text_util::ellipsize(text, room)
    };
    let text = format!(" {cut} ");
    let width = text_util::width(&text) as u16;
    if width == 0 || x < clip.x || x + width + 2 > clip.right() {
        return;
    }
    put_arms(buf, clip, x, y, L, style);
    text_util::put(buf, clip, x + 1, y, &text, style);
    put_arms(buf, clip, x + width + 1, y, R, style);
}

/// Put a label that starts with a coloured symbol and continues in a second style.
#[allow(clippy::too_many_arguments)]
fn border_label_parts(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    head: &str,
    head_style: Style,
    tail: &str,
    tail_style: Style,
    max_width: usize,
) {
    let head = text_util::ellipsize(head, max_width);
    let head_width = text_util::width(&head) as u16;
    let room = max_width.saturating_sub(head_width as usize + 1);
    let tail = text_util::ellipsize(tail, room);
    let plain = head_width + 1 + text_util::width(&tail) as u16;
    let pad = if plain + 2 <= max_width as u16 { 1 } else { 0 };
    let width = plain + 2 * pad;
    if width == 0 || x + width + 2 > clip.right() {
        return;
    }
    put_arms(buf, clip, x, y, L, head_style);
    let mut at = x + 1;
    if pad == 1 {
        text_util::put(buf, clip, at, y, " ", head_style);
        at += 1;
    }
    text_util::put(buf, clip, at, y, &head, head_style);
    at += head_width;
    text_util::put(buf, clip, at, y, " ", tail_style);
    at += 1;
    text_util::put(buf, clip, at, y, &tail, tail_style);
    at += text_util::width(&tail) as u16;
    if pad == 1 {
        text_util::put(buf, clip, at, y, " ", tail_style);
    }
    put_arms(buf, clip, x + width + 1, y, R, tail_style);
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "agent" } else { "agents" }
}

fn repositories_plural(count: usize) -> &'static str {
    if count == 1 {
        "repository"
    } else {
        "repositories"
    }
}

/// The repository line: the count always, whole names while they fit, and `(+N)`
/// for the rest. A name is never cut to `tea…`: a person must be able to tell how
/// many repositories there are and which ones are hidden (F-09).
fn repository_line(view: &ForgejoView, room: usize) -> String {
    let names = if view.repositories == 0 {
        view.repository_names.as_slice()
    } else {
        &view.repository_names[..view.repository_names.len().min(view.repositories)]
    };
    let count = view.repositories.max(names.len());
    let base = format!(
        "{} · {} {}",
        view.organisation,
        count,
        repositories_plural(count)
    );
    if names.is_empty() {
        return base;
    }
    let mut shown: Vec<&str> = Vec::new();
    for name in names {
        let mut candidate = base.clone();
        for kept in &shown {
            candidate.push_str(" · ");
            candidate.push_str(kept);
        }
        candidate.push_str(" · ");
        candidate.push_str(name);
        if text_util::width(&candidate) <= room {
            shown.push(name);
        } else {
            break;
        }
    }
    let mut hidden = names.len().saturating_sub(shown.len());
    let mut line = base.clone();
    for kept in &shown {
        line.push_str(" · ");
        line.push_str(kept);
    }
    // Reserve room for `(+N)` by dropping the last shown name: a count with no
    // hint that names are hidden is not the whole truth (NB1).
    while hidden > 0 {
        let tag = format!(" (+{hidden})");
        if text_util::width(&line) + text_util::width(&tag) <= room {
            line.push_str(&tag);
            break;
        }
        if shown.pop().is_none() {
            break;
        }
        hidden += 1;
        line = base.clone();
        for kept in &shown {
            line.push_str(" · ");
            line.push_str(kept);
        }
    }
    line
}

/// Where Forgejo is, in the box's line, without an address before it is installed.
fn forgejo_address(view: &ForgejoView) -> &str {
    match view.state {
        ForgejoState::Missing | ForgejoState::Fetching { .. } | ForgejoState::Preparing => "—",
        _ => &view.url,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_forgejo_box(
    buf: &mut Buffer,
    rect: Rect,
    view: &ForgejoView,
    agent_count: usize,
    selected: bool,
    wire: bool,
    wire_x: Option<u16>,
    preview: bool,
) {
    if rect.width < 8 || rect.height < 2 {
        return;
    }
    let face = status::forgejo_face(&view.state);
    let border = if selected {
        theme::bold(theme::SELECT)
    } else {
        theme::bold(theme::ACCENT)
    };
    draw_plain_box(buf, rect, border);
    let right = rect.right() - 1;
    let centre: u16 = rect.x + rect.width / 2;

    border_label(
        buf,
        rect,
        rect.x + 1,
        rect.y,
        "FORGEJO",
        theme::bold(theme::ACCENT),
        rect.width.saturating_sub(10) as usize,
    );
    let (face_symbol, face_status, face_colour) = if preview {
        ('◐', "will create".to_owned(), theme::AMBER)
    } else {
        (face.symbol, face.status.clone(), face.colour)
    };
    let state_text = if preview {
        format!("{face_symbol} {face_status}")
    } else {
        format!("{face_symbol} {face_status} · {}", view.version)
    };
    let state_width = text_util::width(&state_text) as u16 + 2;
    if state_width + 6 < rect.width {
        let state_x = right.saturating_sub(state_width + 3);
        border_label(
            buf,
            rect,
            state_x,
            rect.y,
            &state_text,
            theme::bold(face_colour),
            rect.width.saturating_sub(4) as usize,
        );
    }

    let room = rect.width.saturating_sub(4) as usize;
    // The picker's Forgejo does not exist yet. Speak in the future tense and do not
    // print a login before there is an account to log in with.
    let address = if preview {
        format!("will run at {}", view.url)
    } else {
        forgejo_address(view).to_owned()
    };
    let kept = if view.admin_login_kept { " (kept)" } else { "" };
    let login = if preview {
        format!("will create {}", repository_line(view, room))
    } else {
        format!("your login: {}{kept}", view.admin_login)
    };
    let mut junction_x = wire_x
        .unwrap_or(centre)
        .clamp(rect.x + 3, right.saturating_sub(2));
    match rect.height {
        2 => {
            // The floor: two border rows hold the whole picture. The address, the
            // repository count, the agent count and the login stay; the version is
            // the first fact given up (W67-7).
            // Keep the login and the repository count: they are the two facts a
            // person needs to open Forgejo. The organisation is the first name
            // dropped when the line is tight, then the agent count.
            let mut facts = format!(
                "{address} · {} {}",
                view.repositories,
                repositories_plural(view.repositories)
            );
            let with_agents = format!("{facts} · {agent_count} {}", plural(agent_count));
            if text_util::width(&with_agents) + text_util::width(&login) + 3 <= room {
                facts = with_agents;
            }
            facts.push_str(" · ");
            facts.push_str(&login);
            junction_x = (rect.x + 3 + text_util::width(&facts) as u16 + 2).max(centre);
            border_label(
                buf,
                rect,
                rect.x + 1,
                rect.bottom() - 1,
                &text_util::ellipsize_words(&facts, junction_x.saturating_sub(rect.x + 3) as usize),
                theme::fg(theme::MUTED),
                junction_x.saturating_sub(rect.x + 3) as usize,
            );
        }
        3 => {
            // The floor's short box: the address, the repository count and the
            // agent count on its one interior row, the login on the bottom border.
            // The version is the fact given up first (W67-7).
            let info = format!(
                "{address} · {} · {} {} · {agent_count} {}",
                view.organisation,
                view.repositories,
                repositories_plural(view.repositories),
                plural(agent_count)
            );
            text_util::put(
                buf,
                rect,
                rect.x + 1,
                rect.y + 1,
                &text_util::ellipsize(&info, rect.width.saturating_sub(2) as usize),
                theme::fg(theme::MUTED),
            );
            let label_room = junction_x.saturating_sub(rect.x + 3) as usize;
            border_label(
                buf,
                rect,
                rect.x + 1,
                rect.bottom() - 1,
                &text_util::ellipsize_words(&login, label_room),
                theme::fg(theme::MUTED),
                label_room,
            );
        }
        height => {
            let has_bar = height >= 6;
            let info_x = rect.x + 2;
            let mut fact_y = rect.y + 1;
            if has_bar {
                let bar = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), 1);
                text_util::fill(buf, bar, Style::new().fg(theme::ACCENT).bg(theme::ACCENT));
                fact_y = rect.y + 2;
            }
            // Reserve the agent count (and the version when the bar is there)
            // before the repository names, so a crowded box drops a name rather
            // than halving `7 agents` (F-09).
            // The version is the first fact the box gives up, so the names and
            // the counts stay whole even at 80 columns (W67-7).
            let suffix = format!("{agent_count} {}", plural(agent_count));
            let repo_room = room.saturating_sub(text_util::width(&suffix) + 3);
            let repo = repository_line(view, repo_room);
            let facts = if has_bar {
                vec![address.clone(), repo, suffix]
            } else {
                vec![address.clone(), format!("{repo} · {suffix}")]
            };
            for (index, fact) in facts.iter().enumerate() {
                text_util::put(
                    buf,
                    rect,
                    info_x,
                    fact_y + index as u16,
                    &text_util::ellipsize(fact, room),
                    theme::fg(if index == 0 { theme::FG } else { theme::MUTED }),
                );
            }
            let label_room = junction_x.saturating_sub(rect.x + 3) as usize;
            border_label(
                buf,
                rect,
                rect.x + 1,
                rect.bottom() - 1,
                &text_util::ellipsize_words(&login, label_room),
                theme::fg(theme::MUTED),
                label_room,
            );
        }
    }

    if wire {
        junction_x = junction_x.min(right.saturating_sub(1)).max(rect.x + 2);
        put_arms(
            buf,
            rect,
            junction_x,
            rect.bottom() - 1,
            L | R | D,
            if selected {
                border
            } else {
                theme::fg(theme::WIRE)
            },
        );
    }
}

/// A two-row card: one text line per agent, for the 72-column floor.
///
/// The top border carries the state symbol, the name and the state; the bottom
/// border carries the login and the run total. The card is still a complete box
/// with a wire junction, so the picture stays whole (F-03/U1).
/// The state phrase a narrow board card can hold whole. The Agent panel and the
/// selected-card footer keep the full sentence.
fn board_state(face: &status::Face) -> String {
    match face.status.as_str() {
        "needs an instruction" => "needs instruction".to_owned(),
        "waiting for Forgejo" => "waiting Forgejo".to_owned(),
        other => other
            .strip_prefix("next run in ")
            .map(|rest| format!("next {rest}"))
            .unwrap_or_else(|| other.to_owned()),
    }
}

/// The text a card puts on its bottom border.
///
/// It is built here, once, so the layout can place a single-column wire junction
/// after it and never draw a connector through a word (F-14).
fn card_bottom_text(agent: &AgentView, card_h: u16) -> String {
    if card_h <= 2 {
        let login = if agent.provisioning == Provisioning::Pending {
            "connecting…".to_owned()
        } else {
            agent.login.clone()
        };
        let count = status::run_count(agent);
        let program = status::program_word(agent);
        let mut line = format!("{login} · {program} · {count}");
        if let Some(word) = status::card_word(agent) {
            line.push_str(" · ");
            if let Some(mark) = status::newest_run_mark(agent)
                && agent.leftover.is_none()
                && !word.starts_with(mark)
            {
                line.push(mark);
                line.push(' ');
            }
            line.push_str(&word);
        } else if let Some(mark) = status::newest_run_mark(agent) {
            line.push(' ');
            line.push(mark);
        }
        line
    } else {
        let face = status::agent_face(agent);
        let word = status::card_word(agent);
        if card_h == 3 {
            // A three-row card has no body row for the state word, so the bottom
            // border carries both, tightly. A cleanup word is the shortest thing the
            // card can say and does not repeat the plain `stopped` state. A failed
            // run's word comes first and is the shortest one that still names the
            // HTTP status: a whole-word cut then keeps the status, the fact that must
            // not disappear (F-14).
            if let Some(leftover) = agent.leftover.as_deref() {
                // A three-row card's whole inner border holds the full category;
                // only the two-column list needs the compact word.
                return status::leftover_words(leftover).to_owned();
            }
            match status::tight_failure_word(agent) {
                Some(word) => format!("{word} · {}", board_state(&face)),
                None => match word {
                    Some(word) => {
                        let state = board_state(&face);
                        let mark = status::newest_run_mark(agent)
                            .filter(|mark| !word.starts_with(*mark))
                            .map(|mark| format!("{mark} "))
                            .unwrap_or_default();
                        if state.contains("no work") {
                            // `next 20s·no work` plus the last-work word: the compact
                            // separator keeps both inside the tight three-row label.
                            format!("{} {mark}{word}", state.replace(" · no work", "·no work"))
                        } else {
                            format!("{state} · {mark}{word}")
                        }
                    }
                    None => status::newest_run_mark(agent)
                        .map(|mark| format!("{mark} {}", board_state(&face)))
                        .unwrap_or_else(|| board_state(&face)),
                },
            }
        } else {
            // A five-row card's border has the card's whole inner width; a verbose
            // failure phrase would lose `from the model` there. The short word keeps
            // the HTTP status and the Agent screen keeps the full phrase.
            let word = if matches!(
                agent.recent.last(),
                Some(SessionEnd::Failed { .. } | SessionEnd::TimedOut)
            ) {
                status::tight_failure_word(agent)
            } else {
                word
            };
            if matches!(
                agent.recent.last(),
                Some(SessionEnd::Failed { .. } | SessionEnd::TimedOut)
            ) {
                // The state is on the top border; this border keeps the program and
                // the whole failure sentence.
                return match word {
                    Some(word) => format!("{} {word}", status::program_word(agent)),
                    None => status::program_word(agent),
                };
            }
            let program = status::program_word(agent);
            match word {
                Some(word) => {
                    let mark = status::newest_run_mark(agent)
                        .filter(|_| agent.leftover.is_none())
                        .filter(|mark| !word.starts_with(*mark))
                        .map(|mark| format!("{mark} "))
                        .unwrap_or_default();
                    format!("{program} · {mark}{word}")
                }
                None => {
                    let mark = status::newest_run_mark(agent)
                        .map(|mark| format!("{mark} "))
                        .unwrap_or_default();
                    format!("{program} · {mark}{}", board_state(&face))
                }
            }
        }
    }
}

fn draw_compact_card(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selected: bool,
    junction: bool,
    junction_x: u16,
    label_room: usize,
) {
    let face = status::agent_face(agent);
    let bg = if selected {
        theme::CARD_SELECTED
    } else {
        theme::CARD
    };
    let base = Style::new().fg(theme::FG).bg(bg);
    let border = if selected {
        theme::card_bold(theme::SELECT, selected)
    } else {
        Style::new().fg(theme::WIRE).bg(bg)
    };
    text_util::fill(buf, rect, base);
    let right = rect.right() - 1;
    let bottom = rect.bottom() - 1;
    for x in rect.x + 1..right {
        put_arms(buf, rect, x, rect.y, L | R, border);
        put_arms(buf, rect, x, bottom, L | R, border);
    }
    put_arms(buf, rect, rect.x, rect.y, R | D, border);
    put_arms(buf, rect, right, rect.y, L | D, border);
    put_arms(buf, rect, rect.x, bottom, R | U, border);
    put_arms(buf, rect, right, bottom, L | U, border);

    let title = if selected {
        format!("▸ {}", agent.name)
    } else {
        agent.name.clone()
    };
    let tail = if agent.leftover.is_some() {
        // The bottom border's cleanup word already names the state; repeating it on
        // the top border would spend the narrow line on the same sentence twice.
        title.clone()
    } else {
        format!("{title} · {}", board_state(&face))
    };
    let max_top = (junction_x.saturating_sub(rect.x + 3)) as usize;
    border_label_parts(
        buf,
        rect,
        rect.x + 1,
        rect.y,
        &face.symbol.to_string(),
        theme::card_bold(face.colour, selected),
        &tail,
        theme::card_bold(theme::FG, selected),
        max_top,
    );
    if junction {
        put_arms(
            buf,
            rect,
            junction_x,
            rect.y,
            L | R | U,
            if selected {
                border
            } else {
                Style::new().fg(theme::WIRE).bg(bg)
            },
        );
    }
    // The bottom border carries the identity, the run total and the one last-work
    // (or newest-failure) word, so the floor card keeps all five facts (F-08).
    // The wire junction is placed after this label; cutting on whole words keeps
    // the failure word's HTTP status first, and the junction is never drawn
    // through the text (F-14).
    border_label_tight(
        buf,
        rect,
        rect.x + 1,
        bottom,
        &card_bottom_text(agent, rect.height),
        theme::card(theme::MUTED, selected),
        label_room + 4,
    );
}

/// The bottom label for this card, using the compact cleanup word when the full
/// category cannot fit on this card's border. This keeps `live`, `unowned` and
/// `unverified` distinct instead of cutting them all to `cleanup…`.
fn card_bottom_text_fitted(agent: &AgentView, card_h: u16, max_width: usize) -> String {
    let full = card_bottom_text(agent, card_h);
    // The tight three-row label uses the room as written; the padded label reserves
    // four cells for its cap, spaces and cap.
    let capacity = if card_h <= 2 {
        // The compact bottom border has no junction on it, so it can use the cells
        // the caller reserved for caps and padding.
        max_width.saturating_add(4)
    } else if card_h == 3 {
        max_width
    } else {
        // The padded border label spends four cells on its caps and spaces.
        max_width.saturating_sub(4)
    };
    if let Some(leftover) = agent.leftover.as_deref()
        && text_util::width(&full) > capacity
    {
        return status::leftover_short(leftover).to_owned();
    }
    full
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selected: bool,
    junction: bool,
    junction_x: u16,
    label_room: usize,
    detailed: bool,
) {
    let face = status::agent_face(agent);
    let bg = if selected {
        theme::CARD_SELECTED
    } else {
        theme::CARD
    };
    let base = Style::new().fg(theme::FG).bg(bg);
    let border = if selected {
        theme::card_bold(theme::SELECT, selected)
    } else {
        Style::new().fg(theme::WIRE).bg(bg)
    };
    text_util::fill(buf, rect, base);
    let right = rect.right() - 1;
    let bottom = rect.bottom() - 1;
    for x in rect.x + 1..right {
        put_arms(buf, rect, x, rect.y, L | R, border);
        put_arms(buf, rect, x, bottom, L | R, border);
    }
    for y in rect.y + 1..bottom {
        put_arms(buf, rect, rect.x, y, U | D, border);
        put_arms(buf, rect, right, y, U | D, border);
    }
    put_arms(buf, rect, rect.x, rect.y, R | D, border);
    put_arms(buf, rect, right, rect.y, L | D, border);
    put_arms(buf, rect, rect.x, bottom, R | U, border);
    put_arms(buf, rect, right, bottom, L | U, border);

    let title = if selected {
        format!("▸ {}", agent.name)
    } else {
        agent.name.clone()
    };
    let max_top = (junction_x.saturating_sub(rect.x + 3)) as usize;
    border_label_parts(
        buf,
        rect,
        rect.x + 1,
        rect.y,
        &face.symbol.to_string(),
        theme::card_bold(face.colour, selected),
        &title,
        theme::card_bold(theme::FG, selected),
        max_top,
    );
    if junction {
        put_arms(
            buf,
            rect,
            junction_x,
            rect.y,
            L | R | U,
            if selected {
                border
            } else {
                Style::new().fg(theme::WIRE).bg(bg)
            },
        );
    }
    if rect.height == 3 {
        // A three-row card has no free body row for the program. It rides the top
        // border to the right of the wire, and the bottom keeps the state and the
        // newest work word whole.
        let program = status::program_word(agent);
        let program_width = text_util::width(&program) as u16;
        let label_x = rect.right().saturating_sub(program_width + 5);
        if label_x > junction_x + 1 {
            border_label(
                buf,
                rect,
                label_x,
                rect.y,
                &program,
                theme::card(theme::MUTED, selected),
                program_width as usize + 4,
            );
        }
    }
    let login = if agent.provisioning == Provisioning::Pending {
        "connecting…".to_owned()
    } else {
        agent.login.clone()
    };
    let interior = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    let count = status::run_count(agent);
    let count_width = text_util::width(&count) as u16;
    let login_room = interior.width.saturating_sub(2 + count_width + 1) as usize;
    text_util::put(
        buf,
        interior,
        interior.x + 1,
        rect.y + 1,
        &text_util::ellipsize(&login, login_room.max(1)),
        theme::card(theme::MUTED, selected),
    );
    let count_x = rect.right().saturating_sub(count_width + 2);
    text_util::put(
        buf,
        interior,
        count_x,
        rect.y + 1,
        &count,
        theme::card(theme::MUTED, selected),
    );

    // The state word sits in the card's body from three interior rows up; the
    // bottom border carries the one last-work (or failure) word, so a compact
    // card shows all five facts without a paragraph.
    let state_row = rect.y + 2;
    if rect.height >= 4 {
        // The bottom border already carries the program and the failure word. The
        // state row does not repeat the program; on an error card that read `Pi`
        // twice and crowded the wire.
        text_util::put(
            buf,
            interior,
            interior.x + 1,
            state_row,
            &text_util::ellipsize(
                &board_state(&face),
                interior.width.saturating_sub(1) as usize,
            ),
            theme::card(face.colour, selected),
        );
    }
    if rect.height >= 5 {
        // A roomy card carries the newest activity and its age in its own body row,
        // as the board docs promise; the last-work (or failure) word stays on the
        // bottom border. A card that is not roomy keeps the run's own result line.
        let width = interior.width.saturating_sub(1) as usize;
        let (line, colour) = if let Some(retrying) = status::retry_line(agent) {
            // A 503 being retried outranks the last activity: the run is still
            // working, and the temporary condition is the current fact.
            (Some(retrying), theme::AMBER)
        } else if detailed {
            match status::activity_in(agent, width) {
                Some(line) => (Some(line), theme::MUTED),
                None => (status::run_result_line(agent), status::result_colour(agent)),
            }
        } else if let Some(activity) = status::activity_in(agent, width) {
            // At 80x24 the card is five rows but not `detailed`; the newest activity
            // still owns its allocated body row. The result summary remains in the
            // Agent screen rather than suppressing the activity here (C1).
            (Some(activity), theme::MUTED)
        } else {
            (status::run_result_line(agent), status::result_colour(agent))
        };
        if let Some(line) = line {
            text_util::put(
                buf,
                interior,
                interior.x + 1,
                rect.y + 3,
                &text_util::ellipsize_words(&line, width),
                theme::card(colour, selected),
            );
        }
    }
    let label_style = theme::card(
        if status::last_work_is_failure(agent) {
            theme::RED
        } else {
            theme::MUTED
        },
        selected,
    );
    if rect.height == 3 {
        border_label_tight(
            buf,
            rect,
            rect.x + 1,
            bottom,
            &card_bottom_text_fitted(agent, rect.height, label_room),
            label_style,
            label_room,
        );
    } else {
        border_label_words(
            buf,
            rect,
            rect.x + 1,
            bottom,
            &card_bottom_text_fitted(agent, rect.height, label_room + 4),
            label_style,
            label_room + 4,
        );
    }
}

/// A grid of cards under a Forgejo box, with the trunk, bus and wrapped-row spine.
///
/// One geometry rule holds at every supported size: the grid columns are fixed
/// before any row is drawn, every row starts at the same x, and the spine runs in
/// a gap between columns. A partial last row therefore never moves under the
/// spine, and no connector can cross a card. When the cards cannot keep a whole
/// title and their facts, the same place becomes the compact list form.
struct GridGeometry {
    form: BoardForm,
    columns: usize,
    first_visible: usize,
    visible_rows: usize,
    card_h: u16,
    card_w: u16,
    gap: u16,
    forgejo_h: u16,
    footer_rows: u16,
    row_tops: Vec<u16>,
    bus_ys: Vec<u16>,
    spine_x: u16,
    trunk_x: u16,
    /// The supported roomy picture (100×30 and above): a tall card carries the
    /// newest activity row before the last-work word.
    detailed: bool,
    block_top: u16,
    /// The agent box's outer rectangle in the list form.
    list: Option<Rect>,
    /// Width of one list column's text.
    list_col_w: u16,
    /// Left edge of the first list column's text.
    list_x: u16,
    /// Left edge of the first card (or list box) in the fixed grid.
    grid_start: u16,
}

/// Whether the bottom footer row has something to say: a notice, a waiting
/// sentence, or the negative confinement line.
fn status_row_needed(board: &Board) -> bool {
    board.notice().is_some() || waiting_sentence(board).is_some() || !board.view.confined
}

/// The agents the quit warning names: started, and running or restarting.
fn started_agent_names(board: &Board) -> Vec<&str> {
    board
        .active_started()
        .map(|agent| agent.name.as_str())
        .collect()
}

/// The quit warning, chosen so its wrapped form fits the `budget` rows the
/// geometry can give it. Naming every running agent is preferred; when the
/// names alone would need more rows than the frame can spare, the count keeps
/// what stops and the reopen sentence instead of losing them (F-13).
fn quit_warning_text(board: &Board, width: u16, budget: u16) -> String {
    let started = started_agent_names(board);
    let installing: Vec<&str> = board
        .view
        .programs
        .iter()
        .filter(|program| program.installing.is_some())
        .map(|program| program.label.as_str())
        .collect();
    if started.is_empty() && installing.is_empty() {
        return "quit stops Forgejo; it starts again on reopen — q again quits".to_owned();
    }
    let line = if installing.is_empty() {
        let names = join_names(&started);
        if started.len() == 1 {
            format!("quit stops {names} and Forgejo; they start again on reopen — q again quits")
        } else {
            format!("quit stops {names}, and Forgejo; they start again on reopen — q again quits")
        }
    } else {
        let installs = join_names(&installing);
        let cancel = format!("cancels installing {installs}");
        if started.is_empty() {
            format!("quit {cancel}; the previous version is kept — q again quits")
        } else {
            let names = join_names(&started);
            format!("quit stops {names} and {cancel}; q again quits")
        }
    };
    if text_util::wrap(&line, width.saturating_sub(2) as usize).len() as u16 <= budget {
        return line;
    }
    let noun = if started.len() == 1 {
        "agent"
    } else {
        "agents"
    };
    if started.is_empty() {
        format!(
            "quit cancels {} install{}; the previous version is kept — q again quits",
            installing.len(),
            if installing.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "quit stops {} {noun} and cancels {} install{}; q again quits",
            started.len(),
            installing.len(),
            if installing.len() == 1 { "" } else { "s" }
        )
    }
}

/// How many rows the footer really needs. It is the single count both the
/// geometry and the drawing use: the quit warning owns exactly the rows its
/// wrapped text needs (plus the one notice it would show), the remove
/// confirmation owns its consequence and key lines, and otherwise the two key
/// lines, the status line and the event row are each reserved only when they
/// have something to say. The board above gives up these rows; the footer is
/// never asked to fit into fewer (review-2 blocker).
fn footer_rows_for(
    board: &Board,
    inner: Rect,
    forgejo_h: u16,
    board_floor: u16,
    build: bool,
) -> u16 {
    if build {
        return 2;
    }
    if board.confirm_remove.is_some() {
        return 2;
    }
    if board.confirm_quit {
        let notice = u16::from(board.notice().is_some());
        let budget = inner.height.saturating_sub(notice + board_floor).max(1);
        let line = quit_warning_text(board, inner.width, budget);
        let warning = text_util::wrap(&line, inner.width.saturating_sub(2) as usize)
            .len()
            .max(1) as u16;
        return warning + notice;
    }
    let event = !board.view.forgejo.events.is_empty() && forgejo_h < 8;
    2 + u16::from(status_row_needed(board)) + u16::from(event)
}

/// How many agent rows fit inside the list box: the content rows minus the
/// Forgejo box, the trunk gap and the list box's own two borders.
fn list_available_rows(content_h: u16, forgejo_h: u16, trunk_gap: u16) -> usize {
    content_h.saturating_sub(forgejo_h + trunk_gap + 2).max(1) as usize
}

/// The number of columns a card grid may use: enough cells for a whole title and
/// a login, capped so a wide window does not become a strip of slivers.
fn card_column_limit(usable: u16, gap: u16) -> usize {
    (((usable + gap) / (24 + gap)) as usize).clamp(1, 6)
}

/// The balanced column count for `count` cards: six cards are three and three,
/// seven are four and three, never six and a lonely seventh.
fn balanced_columns(count: usize, limit: usize) -> usize {
    let limit = limit.max(1).min(count.max(1));
    let rows = count.max(1).div_ceil(limit);
    count.max(1).div_ceil(rows).clamp(1, limit)
}

fn home_geometry(board: &Board, area: Rect, roomy: bool, build: bool) -> GridGeometry {
    let inner = inner(area);
    let compact = area.width < 80;
    let agent_count = board.view.agents.len().max(1);
    // Three cells between cards leave a blank on each side of the inter-card trunk,
    // so a connector never touches both card borders at once. A crowded board keeps
    // the two-cell gap: one more column of border is worse than a connector that
    // the compact card form does not draw between those cards at all.
    let gap = if compact {
        0
    } else if agent_count <= 3 {
        3
    } else {
        2
    };
    let usable = inner.width.saturating_sub(2);

    // The Forgejo box keeps its login and repository count at every size; the
    // version is the first fact it gives up. At 72 it is three rows, at 80 four,
    // and from 100 six (an accent bar, address, repositories, agents and version,
    // with the login on the bottom border).
    let forgejo_h = if compact {
        3
    } else if roomy {
        6
    } else if area.width < 100 && agent_count >= 5 {
        3
    } else {
        4
    };
    let trunk_gap = if compact || (area.width < 100 && agent_count >= 5) {
        // A crowded 80-column board needs its rows for cards that carry the program
        // and the newest work. The box keeps its three rows (address, repositories,
        // agents, login); the version and the spare wire row are what give way.
        0
    } else {
        1
    };
    // The smallest board the footer must leave room for: the Forgejo box, the
    // trunk gap, and the compact list's bottom border plus one agent row.
    let board_floor = forgejo_h + trunk_gap + 3;
    let footer_rows = footer_rows_for(board, inner, forgejo_h, board_floor, build);
    let content_h = inner.height.saturating_sub(footer_rows);

    let mut use_list = false;
    let mut columns = 1;
    let mut card_w = usable.min(64);
    let mut card_h: u16 = if compact { 2 } else { 3 };
    let mut row_gap = 0_u16;

    if compact {
        // The floor keeps a short vertical list of two-row cards (the old
        // picture) while it fits; four or more take the compact list, which
        // keeps every fact and every agent on the frame.
        if agent_count <= 3 {
            columns = 1;
            card_h = 2;
            card_w = inner.width.saturating_sub(6).clamp(20, 64);
            let total = forgejo_h + agent_count as u16 * (1 + card_h);
            if total > content_h {
                use_list = true;
            }
        } else {
            use_list = true;
        }
    } else {
        let limit = card_column_limit(usable, gap).min(agent_count);
        columns = balanced_columns(agent_count, limit);
        card_w = (usable.saturating_sub((columns as u16 - 1) * gap) / columns as u16)
            .min(if area.width >= 140 { 60 } else { 40 });
        // A card narrower than this cannot hold the state, the newest work word and
        // the program as whole words; use fewer columns and a taller grid instead.
        while card_w < 26 && columns > 1 {
            columns -= 1;
            card_w = (usable.saturating_sub((columns as u16 - 1) * gap) / columns as u16)
                .min(if area.width >= 140 { 60 } else { 40 });
        }
        let rows = agent_count.div_ceil(columns).max(1);
        let mut card_base = forgejo_h + trunk_gap + rows as u16 * (1 + card_h);
        if card_base > content_h && card_h > 2 {
            // A two-row card still carries every fact (state on the top border;
            // login, program, run count and the newest work on the bottom). Prefer
            // that to dropping to the one-line list when the taller cards miss the
            // frame by a row.
            card_h = 2;
            card_base = forgejo_h + trunk_gap + rows as u16 * (1 + card_h);
        }
        if card_base > content_h {
            use_list = true;
        } else {
            // A little spare height makes the cards a picture; the card stops at
            // five rows so it never turns into blank filler (F-07).
            let spare = content_h - card_base;
            let max_h: u16 = 5;
            let per = (spare / rows as u16).min(max_h.saturating_sub(card_h));
            card_h += per;
            let used = forgejo_h + trunk_gap + rows as u16 * (1 + card_h);
            let left = content_h.saturating_sub(used);
            if rows > 1 {
                row_gap = (left / (rows as u16 - 1)).min(if roomy { 2 } else { 1 });
            }
        }
    }

    let mut layout = if use_list {
        // The list form: one wired box of one-line entries. One column is used
        // whenever every agent and its facts fit; a second (or third) only when
        // the rows do not. At the floor the box spans the inner width so each
        // line still carries the whole login and a last-work word. A mixed board
        // uses the dense identity form: agent-N names abbreviate to N under an
        // explicit N=agent-N rule and each login stays inline, so the two
        // columns can use the full inner width without a gap or a login map.
        let dense_identity = board
            .view
            .agents
            .iter()
            .any(|agent| agent.login != format!("gewu-{}", agent.name));
        let max_cols = if compact { 2 } else { 3 };
        let list_w = if compact {
            inner.width
        } else {
            inner.width.saturating_sub(2)
        };
        let mut chosen: Option<usize> = None;
        for cols in 1..=max_cols.min(agent_count) {
            let rows_total = agent_count.div_ceil(cols);
            let col_w = if dense_identity {
                list_w.saturating_sub(2) / cols as u16
            } else {
                list_text_width(list_w, cols)
            };
            if col_w < 26 {
                continue;
            }
            let available = list_available_rows(content_h, forgejo_h, trunk_gap);
            if rows_total <= available {
                chosen = Some(cols);
                break;
            }
        }
        let cols = chosen.unwrap_or_else(|| {
            // More agents than the frame can hold: keep the widest readable
            // columns and scroll with the selection, as the board always did.
            (1..=max_cols.min(agent_count))
                .rev()
                .find(|cols| {
                    if dense_identity {
                        list_w.saturating_sub(2) / *cols as u16 >= 26
                    } else {
                        list_text_width(list_w, *cols) >= 26
                    }
                })
                .unwrap_or(1)
        });
        let rows_total = agent_count.div_ceil(cols);
        let available = list_available_rows(content_h, forgejo_h, trunk_gap);
        let visible = rows_total.min(available).max(1);
        let list_h = visible as u16 + 2;
        let block = forgejo_h + trunk_gap + list_h;
        let block_top = content_origin(inner.y, content_h, block);
        let list_x = inner.x + inner.width.saturating_sub(list_w) / 2;
        let list = Rect::new(list_x, block_top + forgejo_h + trunk_gap, list_w, list_h);
        let col_w = if dense_identity {
            list_w.saturating_sub(2) / cols as u16
        } else {
            list_text_width(list_w, cols)
        };
        let text_x = list.x + 1;
        let selected_row = board.selection / cols.max(1);
        let mut scroll_row = board.scroll_row;
        if selected_row < scroll_row {
            scroll_row = selected_row;
        }
        let last_row = rows_total.saturating_sub(visible);
        scroll_row = scroll_row.min(last_row);
        if selected_row >= scroll_row + visible {
            scroll_row = selected_row + 1 - visible;
        }
        let first_visible = scroll_row.saturating_mul(cols);
        let trunk_x = list.x + list_w / 2;
        let mut row_tops = Vec::new();
        for row in 0..visible {
            row_tops.push(list.y + 1 + row as u16);
        }
        GridGeometry {
            form: BoardForm::List,
            columns: cols,
            first_visible,
            visible_rows: visible,
            card_h: 1,
            card_w: col_w,
            gap: if dense_identity { 0 } else { 1 },
            forgejo_h,
            footer_rows,
            row_tops,
            bus_ys: Vec::new(),
            spine_x: trunk_x,
            trunk_x,
            block_top,
            list: Some(list),
            list_col_w: col_w,
            list_x: text_x,
            grid_start: list_x,
            detailed: roomy,
        }
    } else {
        let rows = agent_count.div_ceil(columns).max(1);
        let visible = rows;
        let block = forgejo_h
            + trunk_gap
            + visible as u16 * (1 + card_h)
            + (visible.saturating_sub(1) as u16) * row_gap;
        let block_top = content_origin(inner.y, content_h, block);
        let grid_start = card_group_start(inner, columns, card_w, gap);
        let mut row_tops = Vec::new();
        let mut bus_ys = Vec::new();
        for row in 0..visible {
            let bus_y = block_top + forgejo_h + trunk_gap + row as u16 * (1 + card_h + row_gap);
            bus_ys.push(bus_y);
            row_tops.push(bus_y + 1);
        }
        let trunk_x = if columns == 1 {
            card_junction_for(grid_start, card_w, card_h, board)
        } else {
            inner.x + inner.width / 2
        };
        let spine_x = if columns == 1 {
            trunk_x
        } else {
            choose_spine_x(inner, columns, card_w, gap, trunk_x, grid_start)
        };
        GridGeometry {
            form: BoardForm::Cards,
            columns,
            first_visible: 0,
            visible_rows: visible,
            card_h,
            card_w,
            gap,
            forgejo_h,
            footer_rows,
            row_tops,
            bus_ys,
            spine_x,
            trunk_x,
            detailed: roomy,
            block_top,
            list: None,
            list_col_w: 0,
            list_x: 0,
            grid_start,
        }
    };
    layout.visible_rows = layout.visible_rows.max(1);
    layout
}

/// One list column's text width: the inner width, its border, the one-cell gap
/// between columns, and the one-cell margin inside the box.
fn list_text_width(box_w: u16, columns: usize) -> u16 {
    box_w.saturating_sub(2).saturating_sub(columns as u16 - 1) / columns.max(1) as u16
}

/// The wire junction for a single-column grid.
///
/// The trunk is centred when the card's bottom label is short enough; when the
/// label would reach the centre (the floor's identity, run count and failure
/// word), the junction moves to just after the label. A connector is never drawn
/// through a word (F-14), and the picture keeps one straight trunk.
fn card_junction_for(x: u16, width: u16, card_h: u16, board: &Board) -> u16 {
    let mut junction = x + width / 2 + 2;
    for agent in &board.view.agents {
        let label = card_bottom_text(agent, card_h);
        // The label starts on x+1 with a cap. The tight three-row label has no
        // padding; every other label has a space on each side.
        let after = if card_h == 3 { 3 } else { 5 };
        junction = junction.max(x + text_util::width(&label) as u16 + after);
        if card_h <= 2 {
            // A two-row card puts the name and state on the top border. A waiting
            // state is longer than the bottom label, so the junction must clear both
            // labels; otherwise the wait word is ellipsized on a card with room to
            // spare (F3/F8).
            let face = status::agent_face(agent);
            let top = format!("{} {} · {}", face.symbol, agent.name, board_state(&face));
            junction = junction.max(x + text_util::width(&top) as u16 + 5);
        }
    }
    junction.clamp(x + 2, x + width.saturating_sub(2))
}

/// Choose a gap between full-row cards for the wrapped rows' vertical spine.
fn card_group_width(count: usize, card_w: u16, gap: u16) -> u16 {
    if count == 0 {
        return 0;
    }
    count as u16 * card_w + (count as u16 - 1) * gap
}

/// The first card's x, centred with a one-cell margin on both sides. The three-card
/// row already had that margin; every row keeps it.
fn card_group_start(inner: Rect, count: usize, card_w: u16, gap: u16) -> u16 {
    let usable = inner.width.saturating_sub(2);
    let group_w = card_group_width(count, card_w, gap);
    inner.x + 1 + usable.saturating_sub(group_w) / 2
}

fn choose_spine_x(
    _inner: Rect,
    columns: usize,
    card_w: u16,
    gap: u16,
    trunk_x: u16,
    grid_start: u16,
) -> u16 {
    if columns < 2 {
        return trunk_x;
    }
    let mut best = trunk_x;
    let mut best_distance = u16::MAX;
    for column in 0..columns.saturating_sub(1) {
        let gap_start = grid_start + (column as u16 + 1) * card_w + column as u16 * gap;
        let offsets: Vec<u16> = if gap >= 3 {
            // A three-cell gap puts the trunk in the middle, with one blank cell
            // against each card border.
            vec![gap / 2]
        } else {
            (0..gap).collect()
        };
        for offset in offsets {
            let x = gap_start + offset;
            let distance = x.abs_diff(trunk_x);
            if distance < best_distance {
                best_distance = distance;
                best = x;
            }
        }
    }
    best
}

fn card_junction(rect: Rect) -> u16 {
    rect.x + rect.width / 2 + 2
}

/// The trunk leaves the Forgejo box at the window's centre when that is a gap, or
/// at the nearest card junction when the centre runs through a card. The box is
/// then centred on the trunk, so the joint is one clean `┼` rather than a trunk and
/// a drop in adjacent cells.
fn choose_trunk_x(inner: Rect, first_row: usize, card_w: u16, gap: u16) -> u16 {
    let default = inner.x + inner.width / 2;
    if first_row == 0 {
        return default;
    }
    let start = card_group_start(inner, first_row, card_w, gap);
    let mut inside = false;
    for column in 0..first_row {
        let card_x = start + column as u16 * (card_w + gap);
        if default >= card_x && default < card_x + card_w {
            inside = true;
            break;
        }
    }
    if !inside {
        return default;
    }
    let mut best = default;
    let mut best_distance = u16::MAX;
    for column in 0..first_row {
        let junction = start + column as u16 * (card_w + gap) + card_w / 2 + 2;
        let distance = junction.abs_diff(default);
        if distance < best_distance {
            best_distance = distance;
            best = junction;
        }
    }
    best
}

fn row_junctions(inner: Rect, count: usize, card_w: u16, gap: u16) -> Vec<u16> {
    if count == 0 {
        return Vec::new();
    }
    let start = card_group_start(inner, count, card_w, gap);
    (0..count)
        .map(|column| start + column as u16 * (card_w + gap) + card_w / 2 + 2)
        .collect()
}

fn draw_wrapped_row_spine(
    buf: &mut Buffer,
    clip: Rect,
    spine_x: u16,
    from_y: u16,
    to_y: u16,
    style: Style,
) {
    let start = from_y.saturating_add(1);
    let end = to_y.saturating_sub(1);
    if start > end {
        return;
    }
    for y in start..=end {
        put_arms(buf, clip, spine_x, y, U | D, style);
    }
}

/// One agent on a list line: the mark, its name, its Forgejo login and its state.
///
/// The name is never cut; the login is kept when it fits whole, and the state takes
/// the rest of the column. The selected line keeps the `▸` and the tint (B1/B2).
fn draw_list_line(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selected: bool,
    cleanup_codes: bool,
) {
    let face = status::agent_face(agent);
    let bg = if selected {
        theme::CARD_SELECTED
    } else {
        theme::CARD
    };
    text_util::fill(buf, rect, Style::new().fg(theme::FG).bg(bg));
    let max = rect.width as usize;
    let name_style = if selected {
        theme::card_bold(theme::SELECT, true)
    } else {
        theme::card_bold(theme::FG, true)
    };
    let login = if agent.provisioning == Provisioning::Pending {
        "connecting…".to_owned()
    } else {
        agent.login.clone()
    };
    let count = status::run_count(agent);
    let mut word = if cleanup_codes && agent.leftover.is_some() {
        let code = agent
            .leftover
            .as_deref()
            .map(status::leftover_code)
            .unwrap_or("");
        match status::last_work_identity(agent) {
            Some(identity) if !code.is_empty() => Some(format!("{identity} {code}")),
            Some(identity) => Some(identity),
            None if !code.is_empty() => Some(code.to_owned()),
            None => None,
        }
    } else {
        status::list_word(agent)
    };
    // The product's generated login is exactly `gewu-<name>`. On the tightest line
    // the box legend can name that rule instead of repeating the prefix; a
    // persisted non-standard login stays on its own line and never uses that rule.
    let login_derivable = agent.login == format!("gewu-{}", agent.name);
    // A list line uses the same short state words as a card border, and keeps the
    // no-work fact (`next 20s·no work`) inside the tightest two-column width.
    let mut state = if agent.leftover.is_some() {
        // The list word already names the cleanup state; appending the face word
        // would repeat it (`⚠ live leftover live leftover`).
        String::new()
    } else {
        board_state(&face).replace(" · no work", "·no work")
    };
    if !login_derivable
        && !cleanup_codes
        && matches!(
            agent.recent.last(),
            Some(SessionEnd::Failed { .. } | SessionEnd::TimedOut)
        )
        && let (Some(identity), Some(failure)) = (status::last_work_identity(agent), word.as_ref())
    {
        // A persisted login leaves just enough room to keep both the last-work
        // identity and the failure cause; the mark and legend carry `stopped`.
        word = Some(format!("{identity} {failure}"));
        state.clear();
    }
    let build = |selected_mark: bool,
                 run: RunWord,
                 no_space: bool,
                 with_login: bool,
                 with_word: bool,
                 with_state: bool| {
        let mut line = String::new();
        if selected_mark {
            line.push('▸');
        }
        line.push(face.symbol);
        if !no_space {
            line.push(' ');
        }
        line.push_str(&agent.name);
        if with_login {
            line.push(' ');
            line.push_str(&login);
        }
        line.push(' ');
        let run_text = match run {
            RunWord::Full => count.clone(),
            RunWord::Compact => format!("run{}", agent.runs_total),
            RunWord::Bare => agent.runs_total.to_string(),
        };
        line.push_str(&run_text);
        if with_word && let Some(word) = &word {
            line.push(' ');
            line.push_str(word);
        }
        if with_state && !state.is_empty() {
            line.push(' ');
            line.push_str(&state);
        }
        line
    };
    let mut line = build(selected, RunWord::Full, false, true, true, true);
    // Tighten in two stages. First the run label is shortened; then the historical
    // last-work word and the current wait word compete for the last cells. A waiting
    // state wins there (it is what the agent is doing now); a finished state keeps
    // the last-work word (the mark and the box legend still name its state). The
    // login is the last whole fact given up, and only the two-column floor needs it.
    let live_word = !state.is_empty() && state != "stopped";
    let waiting = live_word;
    let mut stages = vec![
        (RunWord::Full, false, true, true, true),
        (RunWord::Compact, false, true, true, true),
        (RunWord::Bare, false, true, true, true),
    ];
    if !login_derivable {
        // A persisted long login may not fit with every other fact. Give up the
        // inline copy before giving up the wait/failure or last-work word; the exact
        // login is named in the list box's top-border identity legend.
        stages.push((RunWord::Bare, false, false, true, true));
    }
    if waiting {
        stages.push((RunWord::Bare, false, true, false, true));
        // The wait word and the last-work word both outrank the login: keep both,
        // with the name and count, before giving up the login to a plain line.
        stages.push((RunWord::Bare, false, false, true, true));
        stages.push((RunWord::Bare, false, true, true, false));
    } else {
        stages.push((RunWord::Bare, false, true, true, false));
        stages.push((RunWord::Bare, false, false, true, true));
        stages.push((RunWord::Bare, false, true, false, true));
    }
    stages.push((RunWord::Bare, false, false, true, false));
    stages.push((RunWord::Bare, false, false, false, true));
    for (run, no_space, with_login, with_word, with_state) in stages {
        if text_util::width(&line) <= max {
            break;
        }
        line = build(selected, run, no_space, with_login, with_word, with_state);
    }
    // The one-line fallback keeps every historical fact first; when the column has
    // room left, the program word is added as the last whole fact. At the crowded
    // 72x18 twelve-agent floor it is omitted rather than losing the login, the run
    // count or the newest work.
    let program = status::program_word(agent);
    if !line.contains(&program) && text_util::width(&line) + 1 + text_util::width(&program) <= max {
        line.push(' ');
        line.push_str(&program);
        if let Some(mark) = status::newest_run_mark(agent).filter(|_| agent.leftover.is_none()) {
            if text_util::width(&line) + 2 <= max {
                line.push(' ');
                line.push(mark);
            }
        }
    }
    if text_util::width(&line) > max
        && let Some(leftover) = agent.leftover.as_deref()
    {
        // The full cleanup word did not fit even without the login. The short word
        // still distinguishes a live process from uncertainty; the Agent screen and
        // the footer carry the full one.
        let short = format!(
            "{}{} {} {} {}",
            if selected { "▸" } else { "" },
            face.symbol,
            agent.name,
            agent.runs_total,
            status::leftover_short(leftover)
        );
        if text_util::width(&short) <= max {
            line = short;
        }
    }
    if text_util::width(&line) > max {
        line = text_util::ellipsize_words(&line, max);
    }
    text_util::put(
        buf,
        rect,
        rect.x,
        rect.y,
        &line,
        if selected {
            name_style
        } else {
            theme::card(theme::FG, false)
        },
    );
}

#[derive(Clone, Copy)]
enum RunWord {
    Full,
    Compact,
    Bare,
}

/// The one-line agent form: one wired box whose interior is a line per agent.
fn draw_agent_list(
    board: &Board,
    buf: &mut Buffer,
    geometry: &GridGeometry,
    forgejo_rect: Rect,
    wire_style: Style,
    selection_x: u16,
) -> BoardLayout {
    let list = geometry.list.expect("the list form has a box");
    for y in forgejo_rect.bottom()..list.y {
        put_arms(buf, buf.area, geometry.trunk_x, y, U | D, wire_style);
    }
    text_util::fill(buf, list, Style::new().fg(theme::FG).bg(theme::CARD));
    draw_plain_box(buf, list, Style::new().fg(theme::WIRE).bg(theme::CARD));

    let mixed = board
        .view
        .agents
        .iter()
        .any(|agent| agent.login != format!("gewu-{}", agent.name));
    let abbreviated_names = board
        .view
        .agents
        .iter()
        .any(|agent| dense_agent_number(&agent.name).is_some());
    let cleanup = board
        .view
        .agents
        .iter()
        .any(|agent| agent.leftover.is_some());
    let has_failure = board
        .view
        .agents
        .iter()
        .any(|agent| status::list_word(agent).is_some_and(|word| word.starts_with('✗')));

    let agent_count = board.view.agents.len();
    let mut layout = BoardLayout {
        columns: geometry.columns,
        first_visible: geometry.first_visible,
        cards: Vec::new(),
        listed: Vec::new(),
        form: BoardForm::List,
        forgejo: Some(forgejo_rect),
        wire_cells: vec![(geometry.trunk_x, list.y.saturating_sub(1))],
        scrolls: agent_count > geometry.visible_rows * geometry.columns,
        footer_rows: geometry.footer_rows,
    };
    for row in 0..geometry.visible_rows {
        let y = geometry.row_tops[row];
        for column in 0..geometry.columns {
            let agent_index = geometry.first_visible + row * geometry.columns + column;
            let Some(agent) = board.view.agents.get(agent_index) else {
                continue;
            };
            let rect = Rect::new(
                geometry.list_x + column as u16 * (geometry.list_col_w + geometry.gap),
                y,
                geometry.list_col_w,
                1,
            );
            let selected = !board.forgejo_selected && agent_index == board.selection;
            if mixed {
                draw_dense_list_line(buf, rect, agent, selection_x, selected);
            } else {
                draw_list_line(buf, rect, agent, selected, false);
            }
            layout.listed.push(CardSlot {
                agent: agent_index,
                rect,
            });
        }
    }

    let legend = if !mixed {
        "● working  ◐ waiting  ○ stopped · login=gewu-<name> · run N"
    } else if cleanup {
        if has_failure {
            "N=agent-N · ●w◐w○s · u=unverified o=unowned l=live · ✗=failed"
        } else {
            "N=agent-N · ●w◐w○s · u=unverified o=unowned l=live · run"
        }
    } else if has_failure {
        "N=agent-N · ●work ◐waiting ○stopped · ✗=failed"
    } else {
        "N=agent-N · ●work ◐waiting ○stopped · run"
    };
    let legend = if abbreviated_names {
        legend.to_owned()
    } else {
        legend.replacen("N=agent-N · ", "", 1)
    };
    border_label(
        buf,
        list,
        list.x + 2,
        list.bottom() - 1,
        &legend,
        theme::fg(theme::MUTED),
        list.width.saturating_sub(6) as usize,
    );
    // The trunk arrives at the middle of the box's top border as one `┴`.
    put_arms(
        buf,
        buf.area,
        geometry.trunk_x,
        list.y,
        L | R | U,
        wire_style,
    );
    layout
}

fn dense_agent_number(name: &str) -> Option<&str> {
    name.strip_prefix("agent-")
        .filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Draw a dense mixed-board row. `agent-N` abbreviates to `N` under the
/// explicit frame rule `N=agent-N`; every other name is printed verbatim. The
/// actual login remains inline, so no login map or token is needed and a third,
/// fourth or later recorded identity cannot lose its association. Facts are
/// packed without padding when the column is tight; no required fact is dropped.
fn draw_dense_list_line(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selection_x: u16,
    selected: bool,
) {
    let face = status::agent_face(agent);
    let prefix = dense_agent_number(&agent.name).unwrap_or(&agent.name);
    let head = format!("{}{}:{}", face.symbol, prefix, agent.login);
    let mut tail = agent.runs_total.to_string();
    if let Some(last) = status::last_work_identity(agent) {
        tail.push_str(&last);
    }
    let failure = status::list_word(agent)
        .filter(|word| word.starts_with('✗'))
        .is_some();
    if failure {
        tail.push('✗');
    }
    if let Some(code) = agent.leftover.as_deref().map(status::leftover_letter) {
        tail.push_str(code);
    }
    let spaced = format!("{head} {tail}");
    let line = if text_util::width(&spaced) <= rect.width as usize {
        spaced
    } else {
        format!("{head}{tail}")
    };
    text_util::put(
        buf,
        rect,
        rect.x,
        rect.y,
        &line,
        if selected {
            theme::card_bold(theme::SELECT, true)
        } else {
            theme::card(theme::FG, false)
        },
    );
    if selected {
        // The row has no spare cell for the usual `▸` at the floor; put the
        // selection mark on the outer border, not through an agent fact.
        let mark = Rect::new(selection_x, rect.y, 1, 1);
        text_util::put(
            buf,
            mark,
            selection_x,
            rect.y,
            "▸",
            theme::card_bold(theme::SELECT, true),
        );
    }
}

fn draw_home(board: &mut Board, buf: &mut Buffer, area: Rect, build: bool) {
    let title = if build {
        "Setting up…"
    } else {
        "gewu-multiverse"
    };
    draw_outer(buf, area, title);
    let inner = inner(area);
    let roomy = area.width >= 100 && area.height >= 30;
    let geometry = home_geometry(board, area, roomy, build);
    let agent_count = board.view.agents.len();
    let forgejo_width = if area.width < 80 {
        inner.width
    } else if roomy {
        (inner.width / 2 + 30).clamp(48, 100) & !1
    } else {
        inner.width.saturating_sub(2)
    };
    let forgejo_width = forgejo_width.min(inner.width);
    let forgejo_x = geometry
        .trunk_x
        .saturating_sub(forgejo_width / 2)
        .clamp(inner.x, inner.right().saturating_sub(forgejo_width));
    let forgejo_rect = Rect::new(
        forgejo_x,
        geometry.block_top,
        forgejo_width,
        geometry.forgejo_h,
    );
    draw_forgejo_box(
        buf,
        forgejo_rect,
        &board.view.forgejo,
        agent_count,
        !build && board.forgejo_selected,
        true,
        Some(geometry.trunk_x),
        false,
    );

    let wire_style = theme::fg(theme::WIRE);
    let layout = if geometry.form == BoardForm::List {
        draw_agent_list(board, buf, &geometry, forgejo_rect, wire_style, area.x)
    } else {
        if geometry.columns > 1
            && let Some(first_bus) = geometry.bus_ys.first()
        {
            for y in forgejo_rect.bottom()..*first_bus {
                put_arms(buf, buf.area, geometry.trunk_x, y, U | D, wire_style);
            }
        }
        let mut layout = BoardLayout {
            columns: geometry.columns,
            first_visible: 0,
            cards: Vec::new(),
            listed: Vec::new(),
            form: BoardForm::Cards,
            forgejo: Some(forgejo_rect),
            wire_cells: Vec::new(),
            scrolls: false,
            footer_rows: geometry.footer_rows,
        };
        for row in 0..geometry.visible_rows {
            let index = row * geometry.columns;
            let in_row = agent_count.saturating_sub(index).min(geometry.columns);
            if in_row == 0 {
                break;
            }
            let y = geometry.row_tops[row];
            let mut junctions = Vec::new();
            let mut rects = Vec::new();
            for column in 0..in_row {
                let agent_index = index + column;
                let Some(agent) = board.view.agents.get(agent_index) else {
                    continue;
                };
                let selected = !board.forgejo_selected && agent_index == board.selection;
                let rect = Rect::new(
                    geometry.grid_start + column as u16 * (geometry.card_w + geometry.gap),
                    y,
                    geometry.card_w,
                    geometry.card_h,
                );
                let title = if selected {
                    format!("▸ {}", agent.name)
                } else {
                    agent.name.clone()
                };
                let junction = if geometry.columns == 1 {
                    geometry.trunk_x
                } else {
                    // On a short card the top border also carries the state (and the
                    // compact form's work word), so the wire must clear the whole
                    // visible label, not just the name.
                    let top_label = if geometry.card_h <= 2 {
                        format!("{} {}", agent.name, board_state(&status::agent_face(agent)))
                    } else {
                        title.clone()
                    };
                    card_junction_after_title(rect, &top_label)
                };
                junctions.push(junction);
                rects.push((agent_index, rect, selected, title, junction));
            }
            // The bus spans every card junction, the spine and (on the first row)
            // the trunk. A partial row keeps the same columns, so the spine stays
            // in a gap and never crosses a card.
            draw_bus_any(
                buf,
                buf.area,
                geometry.bus_ys[row],
                &junctions,
                geometry.spine_x,
                (row == 0).then_some(geometry.trunk_x),
                row > 0,
                row + 1 < geometry.visible_rows,
                wire_style,
            );
            if row + 1 < geometry.visible_rows {
                draw_wrapped_row_spine(
                    buf,
                    buf.area,
                    geometry.spine_x,
                    geometry.bus_ys[row],
                    geometry.bus_ys[row + 1],
                    wire_style,
                );
            }
            if geometry.columns == 1 {
                let from = if row == 0 {
                    forgejo_rect.bottom()
                } else {
                    geometry.row_tops[row - 1] + geometry.card_h
                };
                for wire_y in from..y {
                    put_arms(buf, buf.area, junctions[0], wire_y, U | D, wire_style);
                }
                if row > 0 {
                    put_arms(
                        buf,
                        buf.area,
                        junctions[0],
                        geometry.row_tops[row - 1] + geometry.card_h - 1,
                        L | R | D,
                        wire_style,
                    );
                }
            }
            for (agent_index, rect, selected, _title, junction) in rects {
                let Some(agent) = board.view.agents.get(agent_index) else {
                    continue;
                };
                // The number of content cells the bottom label may use. On a
                // single-column grid the wire lands on that border, so the label
                // stops before the junction; there the three-row card uses the
                // tight (unpadded) label so it keeps the state and HTTP status.
                // On a multi-column grid the bottom border keeps its full width.
                let label_room = if geometry.columns == 1 {
                    if rect.height == 3 {
                        junction.saturating_sub(rect.x + 3) as usize
                    } else {
                        junction.saturating_sub(rect.x + 5) as usize
                    }
                } else if rect.height == 3 {
                    // A three-row card's bottom border has no junction on a
                    // multi-column grid: use its whole inner width, so the wait
                    // word and the short last-work word both fit.
                    rect.width.saturating_sub(2) as usize
                } else if rect.height <= 2 {
                    rect.width.saturating_sub(8) as usize
                } else {
                    rect.width.saturating_sub(6) as usize
                };
                if rect.height <= 2 {
                    draw_compact_card(buf, rect, agent, selected, true, junction, label_room);
                } else {
                    draw_card(
                        buf,
                        rect,
                        agent,
                        selected,
                        true,
                        junction,
                        label_room,
                        geometry.detailed,
                    );
                }
                layout.cards.push(CardSlot {
                    agent: agent_index,
                    rect,
                });
                layout.wire_cells.push((junction, y.saturating_sub(1)));
            }
        }
        layout
    };
    board.last = layout;
    board.columns = geometry.columns;
    board.scroll_row = geometry.first_visible / geometry.columns.max(1);

    if build {
        draw_build_footer(board, buf, inner);
    } else {
        draw_home_footer(board, buf, inner);
    }
}

/// The junction on a card's top border, just after its title. Placing it after
/// the name (rather than half-way across the card) keeps the whole title and the
/// whole state word even when a crowded row makes the cards narrow.
fn card_junction_after_title(rect: Rect, title: &str) -> u16 {
    let width = text_util::width(title) as u16;
    (rect.x + 2 + 1 + width + 3).clamp(rect.x + 6, rect.right().saturating_sub(3))
}

/// The picker's bus: the same drawing as the board's, with the old argument list.
#[allow(clippy::too_many_arguments)]
fn draw_bus(
    buf: &mut Buffer,
    clip: Rect,
    bus_y: u16,
    centres: &[u16],
    spine_x: u16,
    trunk_x: Option<u16>,
    spine_from_above: bool,
    spine_below: bool,
    style: Style,
) {
    draw_bus_any(
        buf,
        clip,
        bus_y,
        centres,
        spine_x,
        trunk_x,
        spine_from_above,
        spine_below,
        style,
    );
}

/// Draw a bus across any set of junctions, including a spine and trunk that may
/// lie outside the outermost card.
#[allow(clippy::too_many_arguments)]
fn draw_bus_any(
    buf: &mut Buffer,
    clip: Rect,
    bus_y: u16,
    centres: &[u16],
    spine_x: u16,
    trunk_x: Option<u16>,
    spine_from_above: bool,
    spine_below: bool,
    style: Style,
) {
    if centres.is_empty() {
        return;
    }
    let mut lo = *centres.iter().min().unwrap_or(&spine_x).min(&spine_x);
    let mut hi = *centres.iter().max().unwrap_or(&spine_x).max(&spine_x);
    if let Some(trunk_x) = trunk_x {
        lo = lo.min(trunk_x);
        hi = hi.max(trunk_x);
    }
    let mut arms = vec![L | R; (hi - lo + 1) as usize];
    let index = |x: u16| (x - lo) as usize;
    arms[index(lo)] &= !L;
    arms[index(hi)] &= !R;
    for centre in centres {
        arms[index(*centre)] |= D;
    }
    let at = &mut arms[index(spine_x)];
    if spine_from_above {
        *at |= U;
    }
    if spine_below {
        *at |= D;
    }
    if let Some(trunk_x) = trunk_x
        && trunk_x >= lo
        && trunk_x <= hi
    {
        arms[index(trunk_x)] |= U;
    }
    for (offset, arms) in arms.into_iter().enumerate() {
        put_arms(buf, clip, lo + offset as u16, bus_y, arms, style);
    }
}

/// The two footer lines of a removal confirmation: what it does, and the keys.
fn remove_confirmation(name: &str) -> (String, String) {
    (
        format!("remove {name}? login revoked, account deactivated, history kept"),
        "y remove   n keep".to_owned(),
    )
}

/// Join agent names the way the quit warning names them: `a`, `a and b`,
/// `a, b and c`.
fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = names.split_last().expect("a non-empty list");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

fn draw_home_footer(board: &mut Board, buf: &mut Buffer, inner: Rect) {
    // Exactly the rows `footer_rows_for` reserved; drawing never writes below
    // this, and every line the footer draws has a reserved row (review-2).
    let footer_rows = board.last.footer_rows;
    let footer_top = inner.bottom().saturating_sub(footer_rows);
    if board.confirm_quit {
        let notice = board.notice();
        let width = inner.width.saturating_sub(2) as usize;
        // The warning owns everything the geometry gave it except the notice's
        // one row: consequence, reopen sentence and confirmation are whole.
        let budget = footer_rows.saturating_sub(u16::from(notice.is_some()));
        let line = quit_warning_text(board, inner.width, budget);
        let parts = text_util::wrap(&line, width);
        for (offset, part) in (0..budget).zip(parts) {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                footer_top + offset,
                &part,
                theme::bold(theme::AMBER),
            );
        }
        if let Some(notice) = notice {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                footer_top + footer_rows.saturating_sub(1),
                &text_util::ellipsize(notice, inner.width.saturating_sub(2) as usize),
                theme::fg(theme::FG),
            );
        }
        return;
    }

    if let Some(name) = &board.confirm_remove {
        // The consequences and the keys are both in the footer: a confirmation
        // without its verb is a screen the person has to guess at (F6).
        let (consequence, keys) = remove_confirmation(name);
        let width = inner.width.saturating_sub(2) as usize;
        for (offset, part) in text_util::wrap(&consequence, width)
            .into_iter()
            .take(1)
            .enumerate()
        {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                footer_top + offset as u16,
                &part,
                theme::bold(theme::AMBER),
            );
        }
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            footer_top + 1,
            &keys,
            theme::bold(theme::AMBER),
        );
        return;
    }
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        footer_top,
        "←→ pick   ⏎ open   s start/stop   + add   − remove",
        theme::fg(theme::MUTED),
    );
    let second = "f Forgejo   m model   ? help   q quit";
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        footer_top + 1,
        second,
        theme::fg(theme::MUTED),
    );
    let layout = &board.last;
    let box_shows_event = layout.forgejo.map(|rect| rect.height >= 8).unwrap_or(false);
    let event = if box_shows_event {
        None
    } else {
        board.view.forgejo.events.last()
    };
    // B3: the event owns a row. It is never right-aligned onto the key line, so a
    // long repository name or the scroll indicator cannot draw over a key hint.
    // The event row sits directly under the two key lines; the status sentence
    // (when one is drawn) takes the row after it. `footer_rows_for` reserved
    // exactly these rows, so the event is never placed on the status row and the
    // status is never written past the reserved block (review-2).
    let event_y = footer_top + 2;
    let status_y = footer_top + 2 + u16::from(event.is_some());
    let shown = if layout.form == BoardForm::List {
        layout.listed.len()
    } else {
        layout.cards.len()
    };
    let mut right = String::new();
    if layout.scrolls {
        let first = layout.first_visible + 1;
        let last = (layout.first_visible + shown).min(board.view.agents.len());
        right.push_str(&format!(
            "{}{}–{} / {} {}",
            if first > 1 { "↑" } else { "" },
            first,
            last.max(first),
            board.view.agents.len(),
            if last < board.view.agents.len() {
                "↓"
            } else {
                ""
            }
        ));
    }
    if !right.is_empty() {
        // The indicator stays right of the second key line and gives way rather
        // than overwrite it.
        let keys_width = text_util::width(second) as u16;
        let room = inner.width.saturating_sub(2).saturating_sub(keys_width + 2) as usize;
        if room >= 6 {
            let line = text_util::ellipsize(&right, room);
            let x = inner
                .right()
                .saturating_sub(text_util::width(&line) as u16 + 2);
            text_util::put(
                buf,
                inner,
                x,
                footer_top + 1,
                &line,
                theme::fg(theme::MUTED),
            );
        }
    }
    if let Some(event) = event {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            event_y,
            &text_util::ellipsize(
                &status::plain(event),
                inner.width.saturating_sub(2) as usize,
            ),
            theme::fg(theme::MUTED),
        );
    }
    // The status line owns the row the geometry reserved after the event, if
    // any was reserved; it is never written into an unreserved row.
    let status_reserved = footer_rows > 2 + u16::from(event.is_some());
    if status_reserved {
        if let Some(notice) = board.notice() {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                status_y,
                &text_util::ellipsize(notice, inner.width.saturating_sub(2) as usize),
                theme::fg(theme::FG),
            );
        } else if let Some(line) = waiting_sentence(board) {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                status_y,
                &text_util::ellipsize(&line, inner.width.saturating_sub(2) as usize),
                theme::fg(theme::AMBER),
            );
        } else if !board.view.confined {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                status_y,
                "runs are not confined on this computer",
                theme::fg(theme::AMBER),
            );
        }
    }
}

/// A started agent that cannot run says why in the footer, in the same words as the
/// card, instead of a bare `started`.
fn waiting_sentence(board: &Board) -> Option<String> {
    let agent = board.selected_agent()?;
    let SessionState::Blocked { reason } = &agent.session else {
        return None;
    };
    Some(match status::blocked_words(reason) {
        "needs an instruction" => "not started — write an instruction first".to_owned(),
        other => format!("not started — {other}"),
    })
}

fn draw_build_footer(board: &mut Board, buf: &mut Buffer, inner: Rect) {
    let y = inner.bottom().saturating_sub(2);
    let line = match &board.view.forgejo.state {
        ForgejoState::Failed { reason } => {
            format!("✗ {}   r retry   q quit", status::plain(reason))
        }
        _ => "q quit".to_owned(),
    };
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        y,
        &text_util::ellipsize(&line, inner.width.saturating_sub(2) as usize),
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize(notice, inner.width.saturating_sub(2) as usize),
            theme::fg(theme::FG),
        );
    }
}

fn draw_terms(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "Before you start");
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);

    let width = inner.width.saturating_sub(10) as usize;
    let mut blocks: Vec<(usize, Vec<String>)> = board
        .view
        .terms
        .iter()
        .map(|item| {
            let lines = text_util::wrap(item, width);
            (lines.len().max(1), lines)
        })
        .collect();
    // F-06: the second item is conditional; this status line says what is true on
    // *this* computer. It is not numbered, and it scrolls with item 2. The negative
    // sentence is the engine's sixth term on a host with no mechanism.
    if let Some(method) = &board.view.confinement {
        // The fact about this computer comes first under its item, then the
        // conditional sentence; both are item 2's block, not a sixth warning (F-11).
        let line = format!("This computer confines runs ({method})");
        let mut lines = text_util::wrap(&line, width);
        if let Some((count, block)) = blocks.get_mut(1) {
            lines.extend(block.iter().cloned());
            *block = lines;
            *count = block.len();
        } else {
            blocks.push((lines.len().max(1), lines));
        }
    }
    let total_lines: usize = blocks.iter().map(|(count, _)| *count).sum();
    // When the warnings just fit at the 72x18 floor there is no row left for a blank
    // separator; the numbered items still read as separate items. Never trade an
    // unseen warning for spacing.
    let spacing = if blocks.len() > 1 {
        ((content_h as usize).saturating_sub(total_lines) / (blocks.len() - 1)).clamp(0, 1)
    } else {
        1
    };
    let block_h = total_lines + spacing * blocks.len().saturating_sub(1);
    let mut y = content_origin(content_top, content_h, block_h as u16);
    let offset = board.terms_offset.min(blocks.len().saturating_sub(1));
    board.terms_offset = offset;
    let mut drawn_all = vec![false; blocks.len()];
    let mut more_below = false;
    for (index, (line_count, lines)) in blocks.iter().enumerate() {
        if index < offset {
            continue;
        }
        if y + *line_count as u16 > content_bottom {
            more_below = true;
            break;
        }
        text_util::put(
            buf,
            inner,
            inner.x + 3,
            y,
            &format!("{}", index + 1),
            theme::bold(theme::ACCENT),
        );
        let mut drawn = true;
        for (line_index, line) in lines.iter().enumerate() {
            let row = y + line_index as u16;
            if row >= content_bottom {
                drawn = false;
                break;
            }
            text_util::put(buf, inner, inner.x + 6, row, line, theme::fg(theme::FG));
        }
        drawn_all[index] = drawn;
        y += *line_count as u16;
        if index + 1 < blocks.len() {
            y += spacing as u16;
        }
    }
    for (index, drawn) in drawn_all.into_iter().enumerate() {
        if drawn {
            if let Some(seen) = board.terms_seen.get_mut(index) {
                *seen = true;
            }
        }
    }
    if offset > 0 {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            content_top,
            "↑",
            theme::fg(theme::MUTED),
        );
    }
    if more_below {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            content_bottom.saturating_sub(1),
            "↓",
            theme::fg(theme::MUTED),
        );
    }
    let count = board.view.terms.len();
    let wording = if count == 1 { "warning" } else { "warnings" };
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        &format!("{count} {wording}   y agree    n leave"),
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

fn draw_model(board: &mut Board, buf: &mut Buffer, area: Rect) {
    let title = match board.model_agent.as_deref() {
        Some(name) => format!("{name}'s model"),
        None => match board.model_origin {
            crate::board::ModelOrigin::Board => "Shared model".to_owned(),
            crate::board::ModelOrigin::FirstRun => "Your model".to_owned(),
        },
    };
    draw_outer(buf, area, &title);
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let form_width = inner.width.saturating_sub(8).clamp(40, 76).min(inner.width);
    let field_gap = if content_h >= 22 { 3 } else { 1 };
    // The test line gets real rows, not one: the provider's whole answer wraps to
    // what the form can hold, and at 80x24 that is the whole refusal (F-07).
    let form_height = (10 + 2 * field_gap + 2 + 6).min(content_h);
    let form = Rect::new(
        inner.x + inner.width.saturating_sub(form_width) / 2,
        content_origin(content_top, content_h, form_height),
        form_width,
        form_height,
    );
    draw_plain_box(buf, form, theme::fg(theme::WIRE));
    let labels = ["API address", "model", "API key"];
    let mut y = form.y + 1;
    for (index, label) in labels.iter().enumerate() {
        let selected = index == board.model_focus;
        let marker = if selected { "▸ " } else { "  " };
        text_util::put(
            buf,
            form,
            form.x + 2,
            y,
            &format!("{marker}{label}"),
            if selected {
                theme::bold(theme::ACCENT)
            } else {
                theme::fg(theme::MUTED)
            },
        );
        y += 1;
        let field = Rect::new(form.x + 2, y, form.width.saturating_sub(4), 1);
        let style = if selected {
            theme::focus_field()
        } else {
            theme::field(theme::MUTED)
        };
        text_util::fill(buf, field, style);
        let editing = selected && board.model_editing;
        let shown = field_text(board, index, editing);
        text_util::put(
            buf,
            field,
            field.x + 1,
            y,
            &text_util::ellipsize(&shown, field.width.saturating_sub(2) as usize),
            style,
        );
        y += 1;
        if index == 0 {
            text_util::put(
                buf,
                form,
                form.x + 2,
                y,
                &text_util::ellipsize(
                    if board.model_agent.is_some() {
                        "for example https://model.example/v1"
                    } else {
                        "for example https://model.example/v1/chat/completions"
                    },
                    form.width.saturating_sub(4) as usize,
                ),
                theme::fg(theme::MUTED),
            );
            y += 1;
        }
        if index == 0 {
            // The protocol decides what the address means. Showing it here keeps
            // the one consequential field beside the address and model instead of
            // one screen away on Program.
            let changeable = board.model_agent.is_some() && {
                let name = board.model_agent.as_deref().unwrap_or_default();
                board
                    .view
                    .agents
                    .iter()
                    .find(|agent| agent.name == name)
                    .and_then(|agent| crate::harness::Program::parse(&agent.program))
                    .is_some_and(|program| program.supported_protocols().len() > 1)
            };
            let text = if changeable {
                format!("protocol: {}   ←→ change", board.model_protocol.label())
            } else {
                format!("protocol: {}", board.model_protocol.label())
            };
            text_util::put(
                buf,
                form,
                form.x + 2,
                y,
                &text_util::ellipsize(&text, form.width.saturating_sub(4) as usize),
                theme::fg(theme::MUTED),
            );
            y += 1;
        }
        if index + 1 < labels.len() {
            y += field_gap;
        }
    }
    let test = if board.model_agent.is_none() && board.model_test_visible() {
        board.view.endpoint.test.clone()
    } else if let Some(name) = board.model_agent.as_deref() {
        if board.model_test_visible() {
            board
                .view
                .agents
                .iter()
                .find(|agent| agent.name == name)
                .map(|agent| agent.model_test.clone())
                .unwrap_or(EndpointTest::NotRun)
        } else {
            EndpointTest::NotRun
        }
    } else {
        EndpointTest::NotRun
    };
    let (line, style) = match &test {
        EndpointTest::NotRun => ("t test".to_owned(), theme::fg(theme::MUTED)),
        EndpointTest::Running => ("◐ testing".to_owned(), theme::fg(theme::AMBER)),
        EndpointTest::Passed { answer } => (format!("✓ {answer}"), theme::bold(theme::RUNNING)),
        EndpointTest::Failed { reason } => (
            format!("✗ {}", status::plain(reason)),
            theme::bold(theme::RED),
        ),
    };
    // A failed test is the one sentence that has to repair the address, so it
    // wraps rather than losing its end. At 80x24 the form has two rows below the
    // fields; the `/v1` repair is already first, and the provider's clause follows
    // on the second row (review B1).
    let room = form.bottom().saturating_sub(1).saturating_sub(y) as usize;
    let parts = text_util::wrap(&line, form.width.saturating_sub(4) as usize);
    for (row, part) in parts.iter().take(room).enumerate() {
        let shown = if row + 1 == room && parts.len() > room {
            format!("{}…", part.trim_end())
        } else {
            part.clone()
        };
        text_util::put(buf, form, form.x + 2, y + row as u16, &shown, style);
    }
    let actions = if board.model_editing {
        "⏎ next field   ctrl-u clear   ↑↓ field   esc cancel"
    } else if board.model_agent.is_some() {
        "e edit   ↑↓ field   t test   ⏎ save   ctrl-u clear   esc back"
    } else {
        "e edit   ↑↓ field   t test   ⏎ next   ctrl-u clear   esc back"
    };
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        &text_util::ellipsize_words(actions, inner.width.saturating_sub(2) as usize),
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

fn field_text(board: &Board, index: usize, editing: bool) -> String {
    let raw = &board.model_fields[index];
    let mut shown = if index == 2 {
        // One mask length in every state: the saved key and the key being edited
        // are both eight bullets, and an empty field is empty — no stray dash
        // (W67-13, F-15).
        let key_present = board
            .model_agent
            .as_deref()
            .and_then(|name| board.view.agents.iter().find(|agent| agent.name == name))
            .map(|agent| agent.key_present)
            .unwrap_or(board.view.endpoint.key_present);
        if raw.is_empty() && (!key_present || board.model_key_cleared) {
            String::new()
        } else {
            "••••••••".to_owned()
        }
    } else {
        raw.clone()
    };
    if editing {
        let cursor = board.model_cursor.min(shown.chars().count());
        let mut chars: Vec<char> = shown.chars().collect();
        chars.insert(cursor, '▏');
        shown = chars.into_iter().collect();
    }
    shown
}

fn draw_picker(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "How many agents?");
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let count = board.agent_count.max(1);

    let preview_gap = 2_u16;
    let max_columns = (((inner.width + preview_gap) / 12).clamp(1, 6) as usize).min(count);
    let columns = max_columns.max(1);
    let rows = count.div_ceil(columns);

    let roomy = area.width >= 100 && area.height >= 30;
    let forgejo_h = if roomy { 6 } else { 3 };
    let trunk_gap = 1_u16;
    let mut card_h = if roomy { 3 } else { 2 };
    let max_card_h: u16 = if roomy {
        if content_h >= 40 { 16 } else { 12 }
    } else {
        5
    };
    let mut row_gap = 0_u16;
    let max_row_gap: u16 = if content_h >= 20 { 2 } else { 0 };
    let base = forgejo_h + trunk_gap + rows as u16 * (1 + card_h);
    let mut spare = content_h.saturating_sub(base);
    let per = (spare / rows as u16).min(max_card_h.saturating_sub(card_h));
    card_h += per;
    spare -= per * rows as u16;
    if rows > 1 {
        let per = (spare / (rows as u16 - 1)).min(max_row_gap.saturating_sub(row_gap));
        row_gap += per;
    }
    let block = forgejo_h + trunk_gap + rows as u16 * (1 + card_h) + (rows as u16 - 1) * row_gap;
    let block_top = content_origin(content_top, content_h, block);
    let forgejo_w = if roomy {
        (inner.width / 2 + 8).clamp(44, 64) & !1
    } else {
        (inner.width.saturating_sub(16)).clamp(36, 50) & !1
    };
    let forgejo_w = forgejo_w.min(inner.width);
    let usable = inner.width.saturating_sub(2);
    let card_w =
        (usable.saturating_sub((columns as u16 - 1) * preview_gap) / columns as u16).clamp(9, 18);
    let first_row = count.min(columns);
    let trunk_x = choose_trunk_x(inner, first_row, card_w, preview_gap);
    let spine_x = choose_spine_x(
        inner,
        columns,
        card_w,
        preview_gap,
        trunk_x,
        card_group_start(inner, columns, card_w, preview_gap),
    );
    let forgejo_x = trunk_x
        .saturating_sub(forgejo_w / 2)
        .clamp(inner.x, inner.right().saturating_sub(forgejo_w));
    let forgejo = Rect::new(forgejo_x, block_top, forgejo_w, forgejo_h);
    draw_forgejo_box(
        buf,
        forgejo,
        &board.view.forgejo,
        count,
        false,
        true,
        Some(trunk_x),
        true,
    );
    let count_y = forgejo.bottom();
    text_util::put(
        buf,
        inner,
        inner.x + 2,
        count_y,
        &format!("−  {}  +", board.agent_count),
        theme::bold(theme::FG),
    );
    put_arms(buf, inner, trunk_x, count_y, U | D, theme::fg(theme::WIRE));
    let mut bus_ys = Vec::new();
    for row in 0..rows {
        let bus_y = count_y + 1 + row as u16 * (1 + card_h + row_gap);
        bus_ys.push(bus_y);
    }
    for (row, bus_y) in bus_ys.iter().enumerate() {
        let start_index = row * columns;
        let in_row = (count - start_index).min(columns);
        if in_row == 0 {
            break;
        }
        let centres = row_junctions(inner, in_row, card_w, preview_gap);
        draw_bus(
            buf,
            inner,
            *bus_y,
            &centres,
            spine_x,
            if row == 0 { Some(trunk_x) } else { None },
            row > 0,
            row + 1 < rows,
            theme::fg(theme::WIRE),
        );
        if row + 1 < rows {
            draw_wrapped_row_spine(
                buf,
                inner,
                spine_x,
                *bus_y,
                bus_ys[row + 1],
                theme::fg(theme::WIRE),
            );
        }
    }
    for (row, bus_y) in bus_ys.iter().enumerate().take(rows) {
        let start_index = row * columns;
        let in_row = (count - start_index).min(columns);
        if in_row == 0 {
            break;
        }
        let start = card_group_start(inner, in_row, card_w, preview_gap);
        for column in 0..in_row {
            let rect = Rect::new(
                start + column as u16 * (card_w + preview_gap),
                bus_y + 1,
                card_w,
                card_h,
            );
            draw_preview_card(buf, rect);
        }
    }
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        "⏎ create   ←→ count   esc back",
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

fn draw_preview_card(buf: &mut Buffer, rect: Rect) {
    let style = Style::new().fg(theme::WIRE).bg(theme::CARD);
    draw_plain_box(buf, rect, style);
    let junction = card_junction(rect);
    put_arms(buf, rect, junction, rect.y, L | R | U, style);
}

fn draw_build_picture(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_home(board, buf, area, true);
}

fn draw_agent(board: &mut Board, buf: &mut Buffer, area: Rect) {
    let Some(agent) = board
        .view
        .agents
        .iter()
        .find(|agent| agent.name == board.agent_name)
        .cloned()
    else {
        draw_home(board, buf, area, false);
        return;
    };
    let agent_title = format!("{} · {}", agent.name, agent.program_label);
    draw_outer(buf, area, &agent_title);
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(3);
    let content_h = content_bottom.saturating_sub(content_top);
    let card_width = inner.width.saturating_sub(8).clamp(40, 90).min(inner.width);
    // The card is as tall as its content, not as tall as the window: the marks sit
    // directly under the instruction instead of at the far edge of thirty blank
    // rows (F-07). A tall window gives a long instruction more display rows; a
    // short one leaves the screen's spare rows outside the card.
    let instruction_rows = agent
        .instruction
        .as_deref()
        .map(|text| {
            text.lines()
                .map(|line| text_util::wrap(line, card_width.saturating_sub(4) as usize).len())
                .sum::<usize>()
                .max(1)
        })
        .unwrap_or(1)
        .min(if content_h < 26 { 4 } else { 16 });
    let card_height = (14 + instruction_rows as u16)
        .clamp(10, content_h.max(10))
        .min(content_h.max(1));
    let card = Rect::new(
        inner.x + inner.width.saturating_sub(card_width) / 2,
        content_top,
        card_width,
        card_height,
    );
    let bg = Style::new().fg(theme::FG).bg(theme::CARD);
    let border = theme::card_bold(theme::SELECT, true);
    text_util::fill(buf, card, bg);
    for x in card.x + 1..card.right() - 1 {
        put_arms(buf, card, x, card.y, L | R, border);
        put_arms(buf, card, x, card.bottom() - 1, L | R, border);
    }
    for y in card.y + 1..card.bottom() - 1 {
        put_arms(buf, card, card.x, y, U | D, border);
        put_arms(buf, card, card.right() - 1, y, U | D, border);
    }
    put_arms(buf, card, card.x, card.y, R | D, border);
    put_arms(buf, card, card.right() - 1, card.y, L | D, border);
    put_arms(buf, card, card.x, card.bottom() - 1, R | U, border);
    put_arms(
        buf,
        card,
        card.right() - 1,
        card.bottom() - 1,
        L | U,
        border,
    );

    // The rule above the run strip is a row of its own: content stops before it and
    // never paints over it (F1). It is only drawn when it cannot cut into the status
    // rows at the top of the smallest card. A narrow card gives the legend two rows
    // so every entry keeps the same word as at a wide size (F-09).
    let narrow_legend = card.width.saturating_sub(4) < 61;
    let legend_rows = if narrow_legend { 2 } else { 1 };
    let strip_y = card.bottom().saturating_sub(2 + legend_rows);
    let rule_y = strip_y.saturating_sub(1);
    let has_rule = rule_y > card.y + 4;
    let divider = |buf: &mut Buffer, y: u16| {
        for x in card.x + 1..card.right() - 1 {
            put_arms(buf, card, x, y, L | R, border);
        }
        put_arms(buf, card, card.x, y, U | D | R, border);
        put_arms(buf, card, card.right() - 1, y, U | D | L, border);
    };
    if has_rule {
        divider(buf, rule_y);
    }

    let face = status::agent_face(&agent);
    let width = card.width.saturating_sub(4) as usize;
    let interior_x = card.x + 2;
    text_util::put(
        buf,
        card,
        interior_x,
        card.y + 1,
        &format!("{} {}", face.symbol, face.word),
        theme::card_bold(face.colour, true),
    );
    let login = text_util::ellipsize(&agent.login, card.width.saturating_sub(4) as usize);
    let login_x = card
        .right()
        .saturating_sub(text_util::width(&login) as u16 + 2);
    text_util::put(
        buf,
        card,
        login_x,
        card.y + 1,
        &login,
        theme::card(theme::MUTED, true),
    );

    // The second row is the one line worth reading beside the status: a failed tool
    // run's cause, else the newest run's own refusal/no-work summary, else the last
    // error, else what it is doing now. Never a stale sentence pretending to be
    // current: `status::activity` adds `last:` and an age when the run is over.
    let activity = status::activity(&agent);
    let retrying = status::retry_line(&agent);
    let failure_mark = status::failure_mark(&agent);
    let failure = if let Some(leftover) = agent.leftover.as_deref() {
        // A cleanup the engine could not finish has its own three state names; the
        // raw engine sentence (and the previous run's failure) is not shown as if it
        // were this run's tool result.
        Some(format!(
            "{failure_mark} {}",
            status::leftover_words(leftover)
        ))
    } else {
        agent
            .last_failure
            .as_ref()
            .map(|failure| {
                let fitted = status::fit_tool_failure(failure, width.saturating_sub(2));
                format!("{failure_mark} {fitted}")
            })
            .or_else(|| {
                agent
                    .last_error
                    .as_ref()
                    .map(|error| format!("✗ {}", status::failure_words(error)))
            })
    };
    let result = status::run_result_line(&agent);
    let failure_lines = failure
        .as_deref()
        .map(|text| text_util::wrap(text, width))
        .unwrap_or_default();
    let result_colour = if status::run_result_is_refusal(&agent) {
        theme::RED
    } else if status::run_result_is_warning(&agent) {
        theme::AMBER
    } else {
        theme::MUTED
    };
    // The chosen program's prerequisite is persistent: the transient status line
    // that used to carry it faded and left only `stopped`. It comes before the run's
    // own rows, and it is wrapped rather than ellipsized because the action at the end
    // is the part a person needs. At the 72x18 floor it may use three rows.
    let program_rows = if let Some(needs) = agent.program_error.as_deref() {
        let lines = text_util::wrap(&format!("⚠ {needs}"), width);
        let room = rule_y.saturating_sub(card.y + 3).clamp(1, 3) as usize;
        let rows = lines.len().min(room);
        for (row, line) in lines.iter().take(rows).enumerate() {
            text_util::put(
                buf,
                card,
                interior_x,
                card.y + 3 + row as u16,
                line,
                theme::card(theme::AMBER, true),
            );
        }
        rows as u16
    } else {
        0
    };
    // The failure and the run's own result answer different questions, and both are
    // drawn: a transient failed tool must not hide the summary beside `✓` (review
    // B2). The activity keeps the row under the status, then the failure, then the
    // result. Active and next settings join them when a save changed the next run.
    let mut extras: Vec<(String, ratatui::style::Style)> = Vec::new();
    // The newest work outranks the run's failure row when the card is short: the
    // last-work line is the persistent fact a person acts on, and the strip keeps
    // the failure mark and word.
    if let Some(result) = result {
        extras.push((result, theme::card(result_colour, true)));
    }
    if let Some(failure) = failure_lines.first() {
        extras.push((failure.clone(), theme::card(theme::RED, true)));
        // A 401/403 sentence keeps its provider clause and its `check the key`
        // action at 80 columns: the rest of the wrapped refusal gets its own row
        // before the result, rather than being cut at the right edge.
        for line in failure_lines.iter().skip(1) {
            extras.push((line.clone(), theme::card(theme::RED, true)));
        }
    }
    // `last work` carries the work; the run's own last sentence is still on the
    // screen, labelled as the result, so a warning is not presented as the work and
    // the model's account is not lost.
    if !agent.last_work.is_empty()
        && let Some(summary) = agent
            .last_work_result
            .as_deref()
            .or(agent.last_result.as_deref())
    {
        extras.push((
            format!("result: {}", status::first_clause(summary)),
            theme::card(result_colour, true),
        ));
    }
    if let Some(changed) = status::active_next_line(&agent) {
        extras.push((changed, theme::card(theme::AMBER, true)));
    }
    let (second, extras) = match (&retrying, &activity, extras.is_empty()) {
        // A retrying model is the live fact the person needs while the run works:
        // name it before the last activity or work row.
        (Some(retrying), _, _) => (
            Some((retrying.clone(), theme::card(theme::AMBER, true))),
            extras,
        ),
        (None, Some(activity), _) => (
            Some((activity.clone(), theme::card(theme::MUTED, true))),
            extras,
        ),
        (None, None, false) => {
            let first = extras.remove(0);
            (Some(first), extras)
        }
        (None, None, true) => (None, extras),
    };
    if let Some((line, style)) = second {
        text_util::put(
            buf,
            card,
            interior_x,
            card.y + 2,
            &text_util::ellipsize_words(&line, width),
            style,
        );
    }
    let extra_top = card.y + 3 + program_rows;
    let extra_capacity = rule_y.saturating_sub(extra_top) as usize;
    let extra_rows = extras.len().min(extra_capacity) as u16;
    for (index, (notice, style)) in extras.iter().take(extra_capacity).enumerate() {
        text_util::put(
            buf,
            card,
            interior_x,
            extra_top + index as u16,
            &text_util::ellipsize_words(notice, width),
            *style,
        );
    }

    // The work block: the last issue / pull request / branch / comment a work run
    // named, correctly typed and labelled `work`, in at most three lines (F5). It is
    // kept until a later run does new work, so a read-only check run does not erase it
    // (W31-3). With no work at all, the Agent screen names the assignment rule.
    let work_start = extra_top + extra_rows;
    let work_start = work_start.min(rule_y);
    let available = if has_rule {
        rule_y.saturating_sub(work_start) as usize
    } else {
        0
    };
    // A roomy card gets the `work` heading and up to three typed rows. A tight card
    // keeps the typed rows but drops the heading, because showing the work matters
    // more than the word and the row still carries its own type (`pull request #5 …`).
    let labelled = available >= 5 && !agent.last_work.is_empty();
    let work_lines = if !agent.last_work.is_empty() {
        3.min(agent.last_work.len())
            .min(available.saturating_sub(if labelled { 3 } else { 2 }))
    } else {
        0
    };
    let no_work = available >= 2 && agent.last_work.is_empty() && agent.no_assigned_work;
    let work_rows = if labelled {
        (work_lines + 1) as u16
    } else if work_lines > 0 {
        work_lines as u16
    } else if no_work {
        if available >= 3 { 2 } else { 1 }
    } else {
        0
    };
    if labelled {
        text_util::put(
            buf,
            card,
            interior_x,
            work_start,
            "work",
            theme::card(theme::MUTED, true),
        );
    }
    let first_work_row = work_start + u16::from(labelled);
    for (index, line) in agent.last_work.iter().take(work_lines).enumerate() {
        text_util::put(
            buf,
            card,
            interior_x,
            first_work_row + index as u16,
            &text_util::ellipsize_words(line, width),
            theme::card(theme::FG, true),
        );
    }
    if no_work {
        text_util::put(
            buf,
            card,
            interior_x,
            work_start,
            &text_util::ellipsize_words(
                &format!("no issues assigned to {} yet", agent.login),
                width,
            ),
            theme::card(theme::AMBER, true),
        );
    }

    let label_y = work_start + work_rows;
    let text_top = label_y + 1;
    let text_bottom = if has_rule { rule_y } else { strip_y };
    let rows = text_bottom.saturating_sub(text_top) as usize;
    if label_y < rule_y {
        text_util::put(
            buf,
            card,
            interior_x,
            label_y,
            "instruction",
            theme::card(theme::MUTED, true),
        );
    }
    match (&agent.instruction, &agent.instruction_first_line) {
        _ if label_y >= rule_y => {}
        (Some(text), _) => draw_instruction(
            buf,
            card,
            interior_x,
            text_top,
            rows,
            width,
            text,
            agent.instruction_lines,
        ),
        (None, Some(line)) => {
            // An older view carries only the first line; the count still says how
            // many file lines the document has.
            let physical = vec![text_util::wrap(line, width)];
            draw_instruction_rows(
                buf,
                card,
                interior_x,
                text_top,
                rows,
                width,
                &physical,
                agent.instruction_lines.max(1),
            );
        }
        (None, None) => {
            if rows > 0 {
                text_util::put(
                    buf,
                    card,
                    interior_x,
                    text_top,
                    &text_util::ellipsize("no instruction — e writes one", width),
                    theme::card_bold(theme::RED, true),
                );
            }
        }
    }

    // The run marks are bounded: five, then `+N`, and a legend. A row of unlabeled
    // marks was a tally nobody could read (F8).
    text_util::put(
        buf,
        card,
        interior_x,
        strip_y,
        "last runs",
        theme::card(theme::MUTED, true),
    );
    let mut x = interior_x + 10;
    let total = agent.recent.len();
    let shown = total.min(5);
    for end in agent.recent.iter().skip(total - shown) {
        let (symbol, colour) = status::session_symbol(end);
        text_util::put(
            buf,
            card,
            x,
            strip_y,
            &symbol.to_string(),
            theme::card(colour, true),
        );
        x += 2;
    }
    let count = status::run_count(&agent);
    let count_x = card
        .right()
        .saturating_sub(text_util::width(&count) as u16 + 2);
    // F-04: the newest mark carries its one word beside it, so `⊘` and `∅` never
    // have to be told apart by shape alone.
    if let Some(newest) = agent.recent.last() {
        let label = status::session_label(newest);
        let (_, colour) = status::session_symbol(newest);
        let room = (count_x.saturating_sub(x + 1)) as usize;
        if text_util::width(label) < room {
            text_util::put(buf, card, x + 1, strip_y, label, theme::card(colour, true));
            x += 1 + text_util::width(label) as u16;
        }
    }
    if total > shown {
        let rest = total - shown;
        text_util::put(
            buf,
            card,
            x,
            strip_y,
            &format!(" +{rest}"),
            theme::card(theme::MUTED, true),
        );
    } else if total == 0 {
        text_util::put(buf, card, x, strip_y, "—", theme::card(theme::MUTED, true));
    }
    text_util::put(
        buf,
        card,
        count_x,
        strip_y,
        &count,
        theme::card(theme::MUTED, true),
    );
    if card.bottom().saturating_sub(1) > strip_y + legend_rows {
        // The same words at every size. A narrow card wraps the legend onto two
        // rows rather than shortening `no work` or `timed out` (F-09).
        let first = "✓ ok ⊘ refused ∅ no work ✗ failed";
        let second = "⚠ cmd ■ stopped ◷ timed out";
        text_util::put(
            buf,
            card,
            interior_x,
            strip_y + 1,
            &text_util::ellipsize(first, width),
            theme::card(theme::MUTED, true),
        );
        if narrow_legend {
            text_util::put(
                buf,
                card,
                interior_x,
                strip_y + 2,
                &text_util::ellipsize(second, width),
                theme::card(theme::MUTED, true),
            );
        } else {
            let one = format!("{first} {second}");
            text_util::put(
                buf,
                card,
                interior_x,
                strip_y + 1,
                &text_util::ellipsize(&one, width),
                theme::card(theme::MUTED, true),
            );
        }
    }

    let actions_y = inner.bottom().saturating_sub(2);
    if let Some(name) = &board.confirm_remove {
        let (consequence, keys) = remove_confirmation(name);
        let width = inner.width.saturating_sub(2) as usize;
        for (offset, part) in text_util::wrap(&consequence, width)
            .into_iter()
            .take(1)
            .enumerate()
        {
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                actions_y.saturating_sub(1) + offset as u16,
                &part,
                theme::bold(theme::AMBER),
            );
        }
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            actions_y,
            &keys,
            theme::bold(theme::AMBER),
        );
    } else {
        let line1 = "s start/stop  e instruction  p program  m model";
        let line2 = "l log  t transcript  j raw  d remove  esc back";
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            actions_y.saturating_sub(1),
            line1,
            theme::fg(theme::MUTED),
        );
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            actions_y,
            line2,
            theme::fg(theme::MUTED),
        );
    }
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

/// Draw the instruction's first rows and a marker counted in **file** lines.
///
/// The first display row is always drawn, even when only one row is available: the
/// instruction must not disappear behind a bare count (U4/F-11). The marker counts
/// the physical lines of the document, not the wrapped rows, and it only appears
/// when a row can be spared for it. At a tall size the spare rows show more lines.
#[allow(clippy::too_many_arguments)]
fn draw_instruction(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    top: u16,
    rows: usize,
    width: usize,
    text: &str,
    file_lines: usize,
) {
    let physical: Vec<Vec<String>> = text
        .lines()
        .map(|line| text_util::wrap(line, width))
        .collect();
    draw_instruction_rows(
        buf,
        clip,
        x,
        top,
        rows,
        width,
        &physical,
        file_lines.max(physical.len()).max(1),
    );
}

/// End a clipped instruction row with an explicit ellipsis.
///
/// The count of hidden **file** lines is preferred when a row can be spared; at the
/// one-row floor there is no such row, so the visible cut itself says the text
/// continues. A row that already ends in an ellipsis is left alone.
fn continued_instruction(part: &str, width: usize) -> String {
    if part.ends_with('…') {
        part.to_owned()
    } else {
        text_util::ellipsize(&format!("{part}…"), width)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_instruction_rows(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    top: u16,
    rows: usize,
    width: usize,
    physical: &[Vec<String>],
    file_lines: usize,
) {
    if rows == 0 {
        return;
    }
    let total_rows: usize = physical.iter().map(Vec::len).sum();
    if total_rows <= rows {
        let mut row = 0;
        let mut last = None;
        for line in physical {
            for part in line {
                let shown = text_util::ellipsize_words(part, width);
                text_util::put(
                    buf,
                    clip,
                    x,
                    top + row as u16,
                    &shown,
                    theme::card(theme::FG, true),
                );
                last = Some((shown, top + row as u16));
                row += 1;
            }
        }
        // A view that carries only the first line still knows how many file lines
        // the document has; name the rest when a row can be spared.
        let hidden = file_lines.saturating_sub(physical.len());
        if hidden > 0 && row < rows {
            let noun = if hidden == 1 { "line" } else { "lines" };
            text_util::put(
                buf,
                clip,
                x,
                top + row as u16,
                &text_util::ellipsize(&format!("… ({hidden} more {noun})"), width),
                theme::card(theme::MUTED, true),
            );
        } else if hidden > 0 {
            // No row is left for the count; the visible cut still says it continues.
            if let Some((part, y)) = last {
                text_util::put(
                    buf,
                    clip,
                    x,
                    y,
                    &continued_instruction(&part, width),
                    theme::card(theme::FG, true),
                );
            }
        }
        return;
    }
    let mut take = rows.saturating_sub(1);
    let marker = take > 0;
    if take == 0 {
        // One row and no room for both: the first line wins, never the count.
        take = 1;
    }
    let mut drawn = 0;
    let mut fully_shown = 0;
    let mut last = None;
    for line in physical {
        if drawn >= take {
            break;
        }
        let mut complete = true;
        for part in line {
            if drawn >= take {
                complete = false;
                break;
            }
            let shown = text_util::ellipsize_words(part, width);
            text_util::put(
                buf,
                clip,
                x,
                top + drawn as u16,
                &shown,
                theme::card(theme::FG, true),
            );
            last = Some((shown, top + drawn as u16));
            drawn += 1;
        }
        if complete {
            fully_shown += 1;
        }
    }
    let hidden = file_lines.saturating_sub(fully_shown);
    if marker && hidden > 0 {
        let noun = if hidden == 1 { "line" } else { "lines" };
        text_util::put(
            buf,
            clip,
            x,
            top + drawn as u16,
            &text_util::ellipsize(&format!("… ({hidden} more {noun})"), width),
            theme::card(theme::MUTED, true),
        );
    } else if hidden > 0 {
        // The one-row floor has no row for the count. End the visible instruction
        // with an explicit cut; the file-line count is still what a spare row would
        // show, never a count of display rows.
        if let Some((part, y)) = last {
            text_util::put(
                buf,
                clip,
                x,
                y,
                &continued_instruction(&part, width),
                theme::card(theme::FG, true),
            );
        }
    }
}

fn draw_program(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "Agent program");
    let inner = inner(area);
    let mut y = inner.y + 1;
    if let Some(unknown) = board.program_unknown.as_ref() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            &text_util::ellipsize(
                &format!("saved program `{unknown}` is not one this product knows"),
                inner.width.saturating_sub(2) as usize,
            ),
            theme::fg(theme::RED),
        );
        y += 1;
    }
    for index in 0..5 {
        let program = if index == 0 {
            Program::Builtin
        } else {
            crate::harness::NATIVE_PROGRAMS
                .get(index - 1)
                .copied()
                .unwrap_or(Program::Builtin)
        };
        let selected = index == board.program_index && board.program_unknown.is_none();
        let marker = if selected { "▸ " } else { "  " };
        let installed = board
            .view
            .programs
            .iter()
            .find(|view| view.id == program.id());
        let state = if !program.is_native() {
            "always ready".to_owned()
        } else if installed.is_some_and(|view| view.installing.is_some()) {
            "installing".to_owned()
        } else if let Some(view) = installed.filter(|view| view.installed) {
            view.installed_version
                .clone()
                .unwrap_or_else(|| view.version.clone())
        } else {
            "not installed".to_owned()
        };
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            &text_util::ellipsize(
                &format!("{marker}{} · {state}", program.label()),
                inner.width.saturating_sub(2) as usize,
            ),
            if selected {
                theme::bold(theme::ACCENT)
            } else {
                theme::fg(theme::MUTED)
            },
        );
        y += 1;
    }
    let program = if board.program_index == 0 {
        Program::Builtin
    } else {
        crate::harness::NATIVE_PROGRAMS
            .get(board.program_index - 1)
            .copied()
            .unwrap_or(Program::Builtin)
    };
    let protocol = board.program_protocol;
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        y,
        &format!("connection: {}", protocol.label()),
        theme::fg(theme::FG),
    );
    y += 1;
    let supported = program.supported_protocols();
    if supported.len() > 1 {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            "←→ change connection",
            theme::fg(theme::MUTED),
        );
    } else if program.is_native() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            "this program uses this connection",
            theme::fg(theme::MUTED),
        );
    }
    y += 1;
    if program == Program::Codex {
        let sandbox = board
            .view
            .agents
            .iter()
            .find(|agent| agent.name == board.agent_name)
            .and_then(|agent| agent.codex_sandbox.clone())
            .unwrap_or_else(|| "native".to_owned());
        let text = if sandbox == "full-access" {
            "Codex sandbox: sandbox off (product Landlock) · press f twice: returns Codex's own sandbox"
        } else {
            "Codex sandbox: its own default · press f twice: removes Codex's own sandbox, keeps product Landlock"
        };
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            &text_util::ellipsize_words(text, inner.width.saturating_sub(2) as usize),
            theme::fg(theme::AMBER),
        );
        y += 1;
    }
    let width = inner.width.saturating_sub(2) as usize;
    if let Some(view) = board
        .view
        .programs
        .iter()
        .find(|view| view.id == program.id())
    {
        let needs_action = program == Program::Dsh && !view.prerequisite_met;
        let detail = if needs_action {
            // The install error used to replace the one row that named the exact
            // floors. Keep the action, and keep the versions it names, in the line
            // that stays on the screen.
            "✗ install Node.js 24.9+ (npm 11.16+), then press i".to_owned()
        } else {
            match (&view.error, view.installing) {
                (Some(error), _) => format!("✗ {} — press i to retry", status::plain(error)),
                (None, Some(Some(percent))) => match view.installing_phase.as_deref() {
                    Some(phase) => format!("◐ {phase} — {percent}%"),
                    None => format!("◐ installing — {percent}%"),
                },
                (None, Some(None)) => match view.installing_phase.as_deref() {
                    Some(phase) => format!("◐ {phase}…"),
                    None => "◐ installing…".to_owned(),
                },
                (None, None) => format!(
                    "{} · {} · about {} MB{}",
                    view.version,
                    view.license,
                    view.download_mb,
                    view.prerequisite
                        .as_deref()
                        .map(|needed| format!(" · needs {needed}"))
                        .unwrap_or_default()
                ),
            }
        };
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            &text_util::ellipsize_words(&detail, width),
            if needs_action {
                theme::fg(theme::RED)
            } else {
                theme::fg(theme::MUTED)
            },
        );
        y += 1;
    }
    if let Some(agent) = board
        .view
        .agents
        .iter()
        .find(|agent| agent.name == board.agent_name)
    {
        let narrow = inner.width < 100;
        let describe = |view: &crate::engine_api::RunProgramView| {
            let version = if view.program == "builtin" {
                "always ready"
            } else {
                view.version.as_str()
            };
            let sandbox = match view.codex_sandbox.as_deref() {
                Some("full-access") => " · sandbox off (product Landlock)",
                _ => "",
            };
            // Program and model first; the address is on the model screen. At 72 and
            // 80 columns the old line spent its width on half an address and cut the
            // model before the person could read it. At the 72 floor the version and
            // protocol step aside so the program, model and opt-out state all stay.
            if narrow {
                format!("{} · {}{}", view.label, view.model, sandbox)
            } else {
                let host = status::host_of(&view.base_url);
                format!(
                    "{} {} · {} · {} @ {}{}",
                    view.label, version, view.protocol, view.model, host, sandbox
                )
            }
        };
        let active = agent
            .active_program
            .as_ref()
            .map(describe)
            .unwrap_or_else(|| "no run yet".to_owned());
        // `next` previews the highlighted program and connection, not the last one
        // saved. Moving the highlight or changing ←→ must update this row before
        // Enter, so the person sees what they are about to use.
        let saved_view = board
            .view
            .programs
            .iter()
            .find(|view| view.id == program.id());
        let state = if !program.is_native() {
            "always ready".to_owned()
        } else if saved_view.is_some_and(|view| view.installing.is_some()) {
            "installing".to_owned()
        } else if let Some(view) = saved_view.filter(|view| view.installed) {
            view.installed_version
                .clone()
                .unwrap_or_else(|| view.version.clone())
        } else {
            "not installed".to_owned()
        };
        // When the highlight is still the saved program and protocol, the line is
        // the exact next run the engine already holds, including its address. A
        // moved highlight or a changed connection is a preview built from the
        // selection, so it updates before Enter.
        let saved_protocol = crate::harness::Protocol::parse(&agent.protocol);
        let preview = if program.id() == agent.program
            && saved_protocol == Some(protocol)
            && let Some(next) = agent.next_program.as_ref()
            && next.program == program.id()
        {
            describe(next)
        } else if narrow {
            format!(
                "{} · {} · {} · {}",
                program.label(),
                state,
                protocol.label(),
                agent.model
            )
        } else {
            format!(
                "{} {} · {} · {} @ {}",
                program.label(),
                state,
                protocol.label(),
                agent.model,
                status::host_of(&agent.base_url)
            )
        };
        let active_word = if matches!(agent.session, SessionState::Running { .. }) {
            "active run"
        } else {
            "latest run"
        };
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y,
            &text_util::ellipsize_words(&format!("{active_word}: {active}"), width),
            theme::fg(theme::MUTED),
        );
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            y + 1,
            &text_util::ellipsize_words(&format!("next run: {preview}"), width),
            theme::fg(theme::MUTED),
        );
        y += 2;
        if program == Program::Builtin {
            let shared = agent
                .next_program
                .as_ref()
                .map(|next| next.model.as_str())
                .unwrap_or_default();
            let saved = agent
                .active_program
                .as_ref()
                .filter(|active| active.program != "builtin")
                .map(|active| {
                    format!(
                        "; your saved {} model {} is kept",
                        active.label, active.model
                    )
                })
                .unwrap_or_default();
            let line = format!("built-in uses shared model {shared}{saved}");
            for (row, part) in text_util::wrap(&line, width)
                .into_iter()
                .take(2)
                .enumerate()
            {
                text_util::put(
                    buf,
                    inner,
                    inner.x + 1,
                    y + row as u16,
                    &part,
                    theme::fg(theme::MUTED),
                );
            }
        }
    }
    // One sentence about what moving does, in the same place on every program and
    // above the keys. It stays on the 72x18 floor, where the extra explanation is
    // exactly what a wary person needs before pressing Enter.
    let keeps = "keeps the instruction, workspace, login and history; applies to the next run";
    let keeps_top = inner.bottom().saturating_sub(4);
    for (row, part) in text_util::wrap(keeps, width)
        .into_iter()
        .take(2)
        .enumerate()
    {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            keeps_top + row as u16,
            &part,
            theme::fg(theme::MUTED),
        );
    }
    let installing = board
        .view
        .programs
        .iter()
        .find(|view| view.id == program.id())
        .is_some_and(|view| view.installing.is_some());
    let actions = if installing {
        "↑↓ program   i cancel install   esc back"
    } else {
        "↑↓ program   ←→ connection   ⏎ use   i install   esc back"
    };
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        &text_util::ellipsize_words(actions, inner.width.saturating_sub(2) as usize),
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

fn draw_forgejo(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "Forgejo");
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let view = &board.view.forgejo;
    let label_x = inner.x + 2;
    let value_x = inner.x + 18;
    let room = inner.width.saturating_sub(20) as usize;
    let repositories = if view.repository_names.is_empty() {
        "—".to_owned()
    } else {
        view.repository_names.join(" · ")
    };
    let team_page = format!(
        "{}/org/{}/teams/{}",
        view.url,
        view.organisation,
        bundled_team()
    );
    let login = if view.admin_login_kept {
        format!("{}  (kept from an earlier setup)", view.admin_login)
    } else {
        view.admin_login.clone()
    };
    // The password is never truncated: when it is shown, it wraps whole. An
    // ellipsis on a secret makes a value that cannot be typed (P3).
    let (password, revealed) = match board.revealed_password() {
        Some(password) => (password.to_owned(), true),
        None => (
            "••••••••   p resets it, c copies — shown for 10 s, old stops".to_owned(),
            false,
        ),
    };
    let agents = if board.view.agents.is_empty() {
        "—".to_owned()
    } else {
        board
            .view
            .agents
            .iter()
            .map(|agent| agent.login.clone())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    // The newest event is the fact a person needs; older ones are history. Keeping
    // one line whole is what lets the floor page carry it at 72x18 (F-05).
    let events = view
        .events
        .last()
        .map(|event| status::plain(event))
        .unwrap_or_default();
    let mut rows: Vec<(&str, Vec<String>)> = vec![
        (
            "address",
            vec![if forgejo_address(view) == "—" {
                "not set up yet".to_owned()
            } else {
                view.url.clone()
            }],
        ),
        ("version", vec![view.version.clone()]),
        (
            "organisation",
            text_util::wrap(
                &format!(
                    "{} — create repositories under {}",
                    view.organisation, view.organisation
                ),
                room,
            ),
        ),
        ("repositories", text_util::wrap(&repositories, room)),
        ("your login:", text_util::wrap(&login, room)),
        ("password", text_util::wrap(&password, room)),
        (
            "agents can",
            text_util::wrap(
                &format!("write to {} repositories only", view.organisation),
                room,
            ),
        ),
        (
            "agents work",
            text_util::wrap(
                &format!(
                    "assigned issues are the focus; comments in cloned {} repos",
                    view.organisation
                ),
                room,
            ),
        ),
        ("team page", text_util::wrap(&team_page, room)),
        ("agents", text_util::wrap(&agents, room)),
    ];
    if !events.is_empty() {
        rows.push(("events", text_util::wrap(&events, room)));
    }
    // The floor keeps every row, including the version, organisation, the assignment
    // rule, the team page and the latest repository event: one compact line each,
    // with no blank row between when the height is tight (F12). The password and
    // login are never dropped either; a row that cannot fit any other way gives up
    // its spacing first.
    let optional = ["team page", "organisation", "version", "agents"];
    loop {
        let lines: usize = rows.iter().map(|(_, lines)| lines.len().max(1)).sum();
        let spacing = if rows.len() > 1 && lines < content_h as usize {
            ((content_h as usize - lines) / (rows.len() - 1)).min(1)
        } else {
            0
        };
        if lines + spacing * rows.len().saturating_sub(1) <= content_h as usize {
            break;
        }
        let Some(index) = rows.iter().position(|(label, _)| optional.contains(label)) else {
            break;
        };
        rows.remove(index);
    }
    let line_count: usize = rows.iter().map(|(_, lines)| lines.len().max(1)).sum();
    let spacing = if rows.len() > 1 && line_count < content_h as usize {
        ((content_h as usize - line_count) / (rows.len() - 1)).min(1)
    } else {
        0
    };
    let block = line_count + spacing * rows.len().saturating_sub(1);
    let mut y = content_origin(content_top, content_h, block as u16);
    for (label, lines) in rows {
        text_util::put(buf, inner, label_x, y, label, theme::fg(theme::MUTED));
        for (index, line) in lines.iter().enumerate() {
            text_util::put(
                buf,
                inner,
                value_x,
                y + index as u16,
                &text_util::ellipsize(line, room),
                if label == "password" && revealed {
                    theme::bold(theme::FG)
                } else {
                    theme::fg(theme::FG)
                },
            );
        }
        y += lines.len().max(1) as u16 + spacing as u16;
    }
    let note = if revealed {
        "the password was reset — the one shown before no longer works"
    } else {
        "open the address in a browser to see their work."
    };
    if y < content_bottom {
        text_util::put(
            buf,
            inner,
            label_x,
            y,
            &text_util::ellipsize(note, inner.width.saturating_sub(4) as usize),
            theme::fg(theme::MUTED),
        );
    }
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        "p reset password   c copy   esc back",
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

/// The team's short name, from the one place that names it.
fn bundled_team() -> &'static str {
    crate::bundled_forgejo::TEAM
}

/// The one screen shown the first time this build opens an older installation.
fn draw_upgrade(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "gewu-multiverse");
    let Some(upgrade) = board.view.upgrade.clone() else {
        return;
    };
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let width = inner.width.saturating_sub(6) as usize;
    let mut lines: Vec<(String, Style, bool)> = vec![
        (
            format!(
                "this installation was {} · this build is {}",
                upgrade.from_version, upgrade.to_version
            ),
            theme::bold(theme::ACCENT),
            true,
        ),
        (
            format!(
                "found: settings · {} · {} · {}",
                crate::words::counted(upgrade.agents, "agent", "agents"),
                crate::words::counted(upgrade.instructions, "instruction", "instructions"),
                crate::words::counted(upgrade.runs, "run", "runs")
            ),
            theme::fg(theme::FG),
            true,
        ),
        (
            "reused: your model, your agents and their names".to_owned(),
            theme::fg(theme::MUTED),
            false,
        ),
        (
            "kept: every instruction and every run".to_owned(),
            theme::fg(theme::MUTED),
            false,
        ),
    ];
    if !upgrade.kept_agents.is_empty() {
        lines.push((
            format!(
                "kept: {}",
                crate::words::counted(upgrade.programs, "installed program", "installed programs")
            ),
            theme::fg(theme::MUTED),
            false,
        ));
    }
    for agent in &upgrade.kept_agents {
        lines.push((
            format!(
                "kept: {} · {} · {} · {}",
                agent.name, agent.program, agent.model, agent.protocol
            ),
            theme::fg(theme::FG),
            false,
        ));
    }
    lines.extend([(
        format!("new: {}", upgrade.whats_new),
        theme::fg(theme::ACCENT),
        true,
    )]);
    if upgrade.warnings_changed {
        lines.push((
            "warnings changed — ⏎ shows them".to_owned(),
            theme::fg(theme::AMBER),
            true,
        ));
    }
    if upgrade.ignored_config {
        lines.push((
            "ignored: the old config.toml (left as it was)".to_owned(),
            theme::fg(theme::MUTED),
            false,
        ));
    }
    lines.push((
        format!("release notes: {}", upgrade.release_url),
        theme::fg(theme::MUTED),
        false,
    ));
    let wrapped: Vec<(Vec<String>, Style)> = lines
        .into_iter()
        .map(|(text, style, _)| (text_util::wrap(&text, width), style))
        .collect();
    let total: usize = wrapped.iter().map(|(lines, _)| lines.len()).sum();
    let rule = content_h as usize;
    let mut y = content_origin(content_top, content_h, total as u16);
    let mut shown = 0_usize;
    for (lines, style) in wrapped {
        for line in lines {
            if shown >= rule.saturating_sub(1) {
                break;
            }
            text_util::put(buf, inner, inner.x + 3, y, &line, style);
            y += 1;
            shown += 1;
        }
        if shown >= rule.saturating_sub(1) {
            break;
        }
    }
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        "⏎ continue",
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

fn draw_help(board: &mut Board, buf: &mut Buffer, area: Rect) {
    draw_outer(buf, area, "Keys");
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let lines = [
        ("board", "←→ pick   ⏎ open   s start/stop"),
        ("", "+ add     − remove   f Forgejo"),
        ("", "m model   ? help   q quit"),
        ("agent", "s start/stop   e instruction   l log"),
        ("", "t transcript   j raw   d remove"),
        ("", "p program   m model   esc back"),
        ("program", "↑↓ program   ←→ connection   ⏎ use   i install"),
        (
            "model",
            "e edit   t test   ⏎ save   ctrl-u clear   esc back",
        ),
    ];
    let line_count = lines.len() as u16;
    let spacing = if line_count > 1 {
        (content_h.saturating_sub(line_count) / (line_count - 1)).clamp(0, 2)
    } else {
        0
    };
    let block = line_count + spacing * line_count.saturating_sub(1);
    let mut y = content_origin(content_top, content_h, block);
    for (group, line) in lines {
        if group.is_empty() {
            text_util::put(
                buf,
                inner,
                inner.x + 12,
                y,
                &text_util::ellipsize(line, inner.width.saturating_sub(14) as usize),
                theme::fg(theme::FG),
            );
        } else {
            let label = text_util::ellipsize(group, 10);
            text_util::put(
                buf,
                inner,
                inner.x + 1,
                y,
                &label,
                theme::bold(theme::ACCENT),
            );
            text_util::put(
                buf,
                inner,
                inner.x + 12,
                y,
                &text_util::ellipsize(line, inner.width.saturating_sub(14) as usize),
                theme::fg(theme::FG),
            );
        }
        y += 1 + spacing;
    }
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        "esc close",
        theme::fg(theme::MUTED),
    );
    if let Some(notice) = board.notice() {
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            inner.bottom().saturating_sub(1),
            &text_util::ellipsize_words(notice, inner.width.saturating_sub(2) as usize),
            theme::bold(theme::AMBER),
        );
    }
}

/// Paint the current screen.
pub fn draw(board: &mut Board, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    text_util::fill(buf, area, theme::base());
    if area.width < 72 || area.height < 18 {
        draw_outer(buf, area, "gewu-multiverse");
        text_util::put(
            buf,
            inner(area),
            area.x + 3,
            area.y + 2,
            "make this window at least 72×18",
            theme::fg(theme::FG),
        );
        return;
    }
    // A wide window gets a capped, centred picture. The cap is 160 columns, not the
    // old 100: past 140 the extra width is used for the repository line and the card
    // detail instead of being left as margin (F8), and only the remainder is centred.
    let area = if area.width > 160 {
        Rect::new(area.x + (area.width - 160) / 2, area.y, 160, area.height)
    } else {
        area
    };
    board.last = BoardLayout::default();
    match board.screen {
        Screen::Terms => draw_terms(board, buf, area),
        Screen::Model => draw_model(board, buf, area),
        Screen::Picker => draw_picker(board, buf, area),
        Screen::Build => draw_build_picture(board, buf, area),
        Screen::Home => draw_home(board, buf, area, false),
        Screen::Agent => draw_agent(board, buf, area),
        Screen::Program => draw_program(board, buf, area),
        Screen::Forgejo => draw_forgejo(board, buf, area),
        Screen::Help => draw_help(board, buf, area),
        Screen::Upgrade => draw_upgrade(board, buf, area),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_arm_set_has_a_character_and_no_other_set_does() {
        for arms in 1_u8..=15 {
            let expected = matches!(
                arms,
                a if a == L | R
                    || a == U | D
                    || a == R | D
                    || a == L | D
                    || a == R | U
                    || a == L | U
                    || a == U | D | R
                    || a == U | D | L
                    || a == L | R | D
                    || a == L | R | U
                    || a == L | R | U | D
                    || a == L
                    || a == R
                    || a == U
                    || a == D
            );
            assert_eq!(arms_char(arms).is_some(), expected, "arms {arms:04b}");
        }
    }

    #[test]
    fn a_narrow_card_state_phrase_is_whole() {
        let face = |status: &str| status::Face {
            symbol: '○',
            word: status.to_owned(),
            colour: theme::GREY,
            status: status.to_owned(),
        };
        assert_eq!(
            board_state(&face("needs an instruction")),
            "needs instruction"
        );
        assert_eq!(board_state(&face("waiting for Forgejo")), "waiting Forgejo");
        assert_eq!(board_state(&face("working")), "working");
        assert_eq!(
            board_state(&face("next run in 20s · no work")),
            "next 20s · no work"
        );
    }

    #[test]
    fn one_repository_is_not_plural() {
        assert_eq!(repositories_plural(1), "repository");
        assert_eq!(repositories_plural(0), "repositories");
        assert_eq!(repositories_plural(2), "repositories");
    }

    #[test]
    fn the_state_faces_have_a_symbol_and_a_word() {
        for state in [
            ForgejoState::Missing,
            ForgejoState::Fetching { percent: Some(42) },
            ForgejoState::Preparing,
            ForgejoState::Starting,
            ForgejoState::Running,
            ForgejoState::Stopped,
            ForgejoState::Failed {
                reason: "x".to_owned(),
            },
        ] {
            let face = status::forgejo_face(&state);
            assert!(!face.word.is_empty());
            assert!(matches!(face.symbol, '●' | '◐' | '○'));
        }
    }
}

#[cfg(test)]
mod program_view_tests {
    use super::*;
    use crate::board::FakeEngine;
    use crate::engine_api::{Command, EngineApi, RunProgramView};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn a_full_access_active_run_stays_visible_beside_a_native_next_at_narrow_widths() {
        for width in [72_u16, 80] {
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
                .expect("agents");
            let mut board = Board::new(engine);
            board.screen = Screen::Program;
            board.agent_name = "agent-1".to_owned();
            // The Program screen preselects the agent's saved program (Codex here);
            // the preview must follow that highlight.
            board.program_index = 2;
            board.program_protocol = crate::harness::Protocol::Responses;
            board.view.agents[0].active_program = Some(RunProgramView {
                program: "codex".to_owned(),
                label: "Codex".to_owned(),
                version: "0.164.0".to_owned(),
                protocol: "responses".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                model: "active-model".to_owned(),
                codex_sandbox: Some("full-access".to_owned()),
            });
            board.view.agents[0].next_program = Some(RunProgramView {
                program: "codex".to_owned(),
                label: "Codex".to_owned(),
                version: "0.164.0".to_owned(),
                protocol: "responses".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                model: "next-model".to_owned(),
                codex_sandbox: Some("native".to_owned()),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).expect("terminal");
            terminal.draw(|frame| board.draw(frame)).expect("draw");
            let buffer = terminal.backend().buffer();
            let mut text = String::new();
            for y in 0..24 {
                for x in 0..width {
                    text.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
                }
                text.push('\n');
            }
            assert!(
                text.contains("sandbox off (product Landlock)"),
                "the active opt-out state was lost at {width} columns: {text}"
            );
            assert!(
                text.contains("next run: Codex") && !text.contains("full-access"),
                "the next native state still read as full-access at {width} columns: {text}"
            );
        }
    }
    #[test]
    fn an_unknown_saved_program_is_shown_and_not_preselected() {
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
            .expect("agents");
        let mut board = Board::new(engine);
        board.screen = Screen::Program;
        board.agent_name = "agent-1".to_owned();
        board.view.agents[0].unknown_program = Some("potato".to_owned());
        board.view.agents[0].program_label = "unknown program `potato`".to_owned();
        board.program_unknown = Some("potato".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| board.draw(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..24 {
            for x in 0..80 {
                text.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
            }
            text.push('\n');
        }
        assert!(text.contains("potato"), "{text}");
        assert!(text.contains("not one this product knows"), "{text}");
        assert!(!text.contains("▸ built-in engine"), "{text}");
    }
}
