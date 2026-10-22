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
    AgentView, Desired, EndpointTest, ForgejoState, ForgejoView, Provisioning, SessionState,
};
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
    /// Rows the footer occupies: three normally, four when a footer event and
    /// the status line both need a row of their own (B3).
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
    let text = format!(
        " {} ",
        text_util::ellipsize(text, max_width.saturating_sub(4))
    );
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

fn draw_forgejo_box(
    buf: &mut Buffer,
    rect: Rect,
    view: &ForgejoView,
    agent_count: usize,
    selected: bool,
    wire: bool,
    preview: bool,
) {
    if rect.width < 8 || rect.height < 3 {
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
    let centre = rect.x + rect.width / 2;

    border_label(
        buf,
        rect,
        rect.x + 1,
        rect.y,
        "FORGEJO",
        theme::bold(theme::ACCENT),
        rect.width.saturating_sub(10) as usize,
    );
    // The picker draws a preview, not a running server: label it so the person cannot
    // mistake the planned state for the real one before `⏎ create` (F14).
    let (face_symbol, face_status, face_colour) = if preview {
        ('◐', "preview".to_owned(), theme::AMBER)
    } else {
        (face.symbol, face.status.clone(), face.colour)
    };
    let state_text = format!("{face_symbol} {face_status}");
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
    if rect.height == 3 {
        // The 72-column floor keeps the address and the live count on its one
        // interior row; the latest event has the footer line when the box cannot
        // hold it (F-05).
        let info = format!(
            "{} · {} {}",
            forgejo_address(view),
            agent_count,
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
    } else {
        // A server front: a solid accent header bar, then the facts in the order a
        // person needs them. Facts that do not fit are dropped whole; no row is left
        // empty. At 80x24 the box is tall enough for the organisation, the
        // repositories, the count, the version, the latest event and the login
        // (F-03/U1, F-05, F-09).
        let bar = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), 1);
        text_util::fill(buf, bar, Style::new().fg(theme::ACCENT).bg(theme::ACCENT));
        let info_x = rect.x + 2;
        let address = if forgejo_address(view) == "—" {
            "—".to_owned()
        } else {
            view.url.clone()
        };
        let mut facts: Vec<String> = vec![
            address,
            repository_line(view, room),
            format!("{} {}", agent_count, plural(agent_count)),
            format!("version {}", view.version),
        ];
        if let Some(event) = view.events.last() {
            facts.push(status::plain(event));
        }
        let kept = if view.admin_login_kept { " (kept)" } else { "" };
        facts.push(format!("your login: {}{kept}", view.admin_login));
        let rows = rect.height.saturating_sub(3) as usize;
        for (index, fact) in facts.iter().take(rows).enumerate() {
            text_util::put(
                buf,
                rect,
                info_x,
                rect.y + 2 + index as u16,
                &text_util::ellipsize(fact, room),
                theme::fg(if index == 0 { theme::FG } else { theme::MUTED }),
            );
        }
    }

    if wire {
        put_arms(
            buf,
            rect,
            centre,
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
fn board_state(face: &status::Face) -> &str {
    match face.status.as_str() {
        "needs an instruction" => "needs instruction",
        "waiting for Forgejo" => "waiting Forgejo",
        other => other,
    }
}

fn draw_compact_card(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selected: bool,
    junction: bool,
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

    let junction_x = card_junction(rect);
    let max_top = (junction_x.saturating_sub(rect.x + 3)) as usize;
    let title = if selected {
        format!("▸ {}", agent.name)
    } else {
        agent.name.clone()
    };
    // The compact border has room for one whole state phrase; the full phrase is
    // on the Agent panel and in the footer sentence.
    let tail = format!("{title} · {}", board_state(&face));
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
    // The second line rides its border: identity and run total, between caps so
    // every arm of the border still meets a neighbour.
    let count = status::run_count(agent);
    let line = format!("{} · {count}", agent.login);
    border_label(
        buf,
        rect,
        rect.x + 1,
        bottom,
        &line,
        theme::card(theme::MUTED, selected),
        rect.width.saturating_sub(4) as usize,
    );
}

fn draw_card(
    buf: &mut Buffer,
    rect: Rect,
    agent: &AgentView,
    selected: bool,
    roomy: bool,
    junction: bool,
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

    let junction_x = card_junction(rect);
    let max_top = (junction_x.saturating_sub(rect.x + 3)) as usize;
    let title = if selected {
        format!("▸ {}", agent.name)
    } else {
        agent.name.clone()
    };
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
    // The run count is on every card, not only the roomy one (N5). On the
    // three-row card it shares the login row, so the login gives way when the two
    // would meet.
    let count = status::run_count(agent);
    let count_width = text_util::width(&count) as u16;
    let login_room = if roomy {
        interior.width.saturating_sub(2) as usize
    } else {
        interior.width.saturating_sub(2 + count_width + 1) as usize
    };
    text_util::put(
        buf,
        interior,
        interior.x + 1,
        rect.y + 1,
        &text_util::ellipsize(&login, login_room.max(1)),
        theme::card(theme::MUTED, selected),
    );
    if !roomy {
        let count_x = rect.right().saturating_sub(count_width + 2);
        text_util::put(
            buf,
            interior,
            count_x,
            rect.y + 1,
            &count,
            theme::card(theme::MUTED, selected),
        );
    }

    if roomy && rect.height >= 5 {
        let status = if roomy {
            face.status.as_str()
        } else {
            board_state(&face)
        };
        text_util::put(
            buf,
            interior,
            interior.x + 1,
            rect.y + 2,
            &text_util::ellipsize(status, interior.width.saturating_sub(1) as usize),
            theme::card(face.colour, selected),
        );
        let count_x = rect.right().saturating_sub(count_width + 2);
        text_util::put(
            buf,
            interior,
            count_x,
            rect.y + 2,
            &count,
            theme::card(theme::MUTED, selected),
        );
        if let Some(result) = status::run_result_line(agent) {
            let colour = if status::run_result_is_refusal(agent) {
                theme::RED
            } else {
                theme::MUTED
            };
            text_util::put(
                buf,
                interior,
                interior.x + 1,
                rect.y + 3,
                &text_util::ellipsize_words(&result, interior.width.saturating_sub(1) as usize),
                theme::card(colour, selected),
            );
        } else if let Some(activity) = status::activity(agent) {
            let lines = text_util::wrap(&activity, interior.width.saturating_sub(1) as usize);
            let take = if rect.height >= 6 { 2 } else { 1 };
            for (index, line) in lines.iter().take(take).enumerate() {
                text_util::put(
                    buf,
                    interior,
                    interior.x + 1,
                    rect.y + 3 + index as u16,
                    line,
                    theme::card(theme::MUTED, selected),
                );
            }
        }
    } else {
        border_label(
            buf,
            rect,
            rect.x + 1,
            bottom,
            board_state(&face),
            theme::card_bold(face.colour, selected),
            rect.width.saturating_sub(2) as usize,
        );
    }
}

/// A grid of cards under a Forgejo box, with the trunk, bus and wrapped-row spine.
///
/// When the cards cannot all fit, the same layout becomes [`BoardForm::List`]: one
/// wired box with a text line per agent, laid out in as many columns as the width
/// allows, so every agent stays on the frame (B1/B2).
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
    block_top: u16,
    /// The agent box's outer rectangle in the list form.
    list: Option<Rect>,
    /// Width of one list column's text.
    list_col_w: u16,
    /// Left edge of the first list column's text.
    list_x: u16,
}

/// Whether the bottom footer row has something to say: a notice, a waiting
/// sentence, or the negative confinement line.
fn status_row_needed(board: &Board) -> bool {
    board.notice().is_some() || waiting_sentence(board).is_some() || !board.view.confined
}

/// How many agent lines fit inside the list box: the content rows minus the
/// Forgejo box, the trunk gap and the list box's own two borders.
fn list_available_rows(content_h: u16, trunk_gap: u16) -> usize {
    content_h.saturating_sub(3 + trunk_gap + 2).max(1) as usize
}

fn home_geometry(board: &Board, area: Rect, roomy: bool, build: bool) -> GridGeometry {
    let inner = inner(area);
    let compact = area.width < 80;
    // The newest event has its own footer row at the floor (B3). Keep the status
    // line too, so the event never shares a row with the keys or the notice.
    let event_in_footer = !build && compact && !board.view.forgejo.events.is_empty();
    let footer_rows = if build {
        2
    } else if event_in_footer && status_row_needed(board) {
        4
    } else {
        3
    };
    let footer_top = inner.bottom().saturating_sub(footer_rows);
    let content_h = footer_top.saturating_sub(inner.y);
    let agent_count = board.view.agents.len();

    // The 72-column floor keeps the picture complete: two-row compact cards for a
    // small team, and one line per agent when the cards would need a scroll (B1).
    // From 80 the three-row card returns and the Forgejo box can grow tall enough
    // for its facts; a crowded board that would cost the box a fact uses the same
    // one-line form (B2).
    let forgejo_h = if roomy { 6 } else { 3 };
    let trunk_gap = if roomy {
        2
    } else if compact {
        0
    } else {
        1
    };
    let card_h = if roomy {
        5
    } else if compact {
        2
    } else {
        3
    };
    let gap = if roomy { 3 } else { 2 };
    let min_card_w = if roomy { 22 } else { 24 };
    let max_columns = if roomy { 6 } else { 4 };
    // Keep one cell between the first card and the frame, and one at the right, the
    // same margin a three-card row has. Without it a four-card row fills the inner
    // width exactly and the first card touches the frame.
    let usable = inner.width.saturating_sub(2);
    let mut columns = (((usable + gap) / (min_card_w + gap)) as usize)
        .clamp(1, max_columns)
        .min(agent_count.max(3))
        .max(1);
    let max_card_w = if roomy {
        if area.width >= 140 { 60 } else { 40 }
    } else {
        30
    };
    let mut card_w =
        ((usable.saturating_sub((columns as u16 - 1) * gap)) / columns as u16).min(max_card_w);
    if card_w < 14 {
        columns = 1;
        card_w = inner.width.saturating_sub(2).min(max_card_w);
    }
    if compact {
        // The floor card keeps the width for the whole `name · state` label; the
        // old 56-cell cap cut `needs an instruction` and `next run in 8s`.
        columns = 1;
        card_w = inner.width.saturating_sub(6).clamp(20, 64);
    }

    // Would every card fit, and would the Forgejo box still reach its facts? The
    // list form is chosen only when a complete card picture is not possible.
    let card_rows_total = agent_count.div_ceil(columns.max(1));
    let capacity = if content_h > forgejo_h + trunk_gap {
        ((content_h - forgejo_h - trunk_gap) / (1 + card_h)).max(1) as usize
    } else {
        1
    };
    let visible_card_rows = card_rows_total.min(capacity).max(1);
    let fact_rows: u16 = 5 + u16::from(!board.view.forgejo.events.is_empty());
    let max_forgejo: u16 = if compact { 3 } else { 3 + fact_rows };
    let card_base = forgejo_h + trunk_gap + visible_card_rows as u16 * (1 + card_h);
    let card_box_h = forgejo_h
        + content_h
            .saturating_sub(card_base)
            .min(max_forgejo.saturating_sub(forgejo_h));
    let cards_fit = card_rows_total <= capacity;
    let facts_fit = card_box_h >= 3 + fact_rows;
    let use_list = if compact {
        !cards_fit
    } else {
        !cards_fit || (!roomy && !facts_fit)
    };

    if use_list {
        // Use the fewest columns that keep two promises: every agent is on the
        // frame, and the Forgejo box still reaches its facts. One column gives each
        // line the full width (mark, name, login, run count and state); two or
        // three are the crowded fallback.
        // At the floor two columns are the most the 72 cells keep readable; a home
        // past the picker's twelve still scrolls there, as it always did. Wider
        // boards may use a third column to keep every name and the box facts.
        let max_list_columns = if compact { 2 } else { 3 };
        let mut list_columns = 1;
        loop {
            let rows = agent_count.div_ceil(list_columns);
            let visible = rows.min(list_available_rows(content_h, trunk_gap));
            let list_h = visible as u16 + 2;
            let spare = content_h.saturating_sub(3 + trunk_gap + list_h);
            let facts_kept = compact || spare >= fact_rows;
            if (rows <= list_available_rows(content_h, trunk_gap) && facts_kept)
                || list_columns >= max_list_columns
            {
                break;
            }
            list_columns += 1;
        }
        let rows_total = agent_count.div_ceil(list_columns);
        let available = list_available_rows(content_h, trunk_gap);
        let visible_rows = rows_total.min(available).max(1);
        let list_w = inner.width.saturating_sub(2).clamp(8, 100);
        let list_x = inner.x + inner.width.saturating_sub(list_w) / 2;
        let col_w = ((list_w.saturating_sub(2)).saturating_sub(list_columns as u16 - 1))
            / list_columns as u16;
        let list_h = visible_rows as u16 + 2;
        let mut forgejo_h = 3;
        let mut spare = content_h.saturating_sub(3 + trunk_gap + list_h);
        let add = spare.min(max_forgejo.saturating_sub(forgejo_h));
        forgejo_h += add;
        spare -= add;
        let block_top = inner.y + spare / 2;
        let list_y = block_top + forgejo_h + trunk_gap;
        let list = Rect::new(list_x, list_y, list_w, list_h);
        let mut row_tops = Vec::new();
        for row in 0..visible_rows {
            row_tops.push(list_y + 1 + row as u16);
        }
        let selected_row = board.selection / list_columns.max(1);
        let mut scroll_row = board.scroll_row;
        if selected_row < scroll_row {
            scroll_row = selected_row;
        }
        let last_row = rows_total.saturating_sub(visible_rows);
        scroll_row = scroll_row.min(last_row);
        if selected_row >= scroll_row + visible_rows {
            scroll_row = selected_row + 1 - visible_rows;
        }
        let first_visible = scroll_row.saturating_mul(list_columns);
        let trunk_x = list_x + list_w / 2;
        return GridGeometry {
            form: BoardForm::List,
            columns: list_columns,
            first_visible,
            visible_rows,
            card_h: 1,
            card_w: col_w,
            gap: 1,
            forgejo_h,
            footer_rows,
            row_tops,
            bus_ys: Vec::new(),
            spine_x: trunk_x,
            trunk_x,
            block_top,
            list: Some(list),
            list_col_w: col_w,
            list_x: list_x + 1,
        };
    }

    let rows_total = agent_count.div_ceil(columns.max(1));
    let mut scroll_row = board.scroll_row;
    let selected_row = board.selection / columns.max(1);
    if selected_row < scroll_row {
        scroll_row = selected_row;
    }
    let last_row = rows_total.saturating_sub(capacity);
    scroll_row = scroll_row.min(last_row);
    if selected_row >= scroll_row + capacity {
        scroll_row = selected_row + 1 - capacity;
    }
    let first_visible = scroll_row.saturating_mul(columns);
    let visible_count = (agent_count.saturating_sub(first_visible)).min(columns * capacity);
    let mut visible_rows = visible_count.div_ceil(columns.max(1));
    if visible_rows == 0 {
        visible_rows = 1;
    }

    let mut forgejo_h = forgejo_h;
    let mut trunk_gap = trunk_gap;
    let mut card_h = card_h;
    let mut row_gap = 0_u16;
    let mut base = forgejo_h + trunk_gap + (visible_rows as u16) * (1 + card_h);
    let mut spare = content_h.saturating_sub(base);
    // The box is only as tall as its facts: address, team/repositories, agents,
    // version, the newest event when there is one, and the login. A taller box would
    // draw empty rows, which the layout tests refuse.
    let max_card: u16 = if roomy {
        if content_h >= 40 { 8 } else { 6 }
    } else if compact {
        2
    } else {
        3
    };
    let max_trunk = if roomy {
        (content_h / 12).clamp(2, 4)
    } else if compact {
        0
    } else {
        1
    };
    let max_row_gap: u16 = if roomy { 2 } else { 0 };

    let add = spare.min(max_forgejo.saturating_sub(forgejo_h));
    forgejo_h += add;
    spare -= add;

    let per = (spare / visible_rows as u16).min(max_card.saturating_sub(card_h));
    card_h += per;
    spare -= per * visible_rows as u16;

    let add = spare.min(max_trunk.saturating_sub(trunk_gap));
    trunk_gap += add;
    spare -= add;

    if visible_rows > 1 {
        let per = (spare / (visible_rows as u16 - 1)).min(max_row_gap.saturating_sub(row_gap));
        row_gap += per;
    }

    base = forgejo_h
        + trunk_gap
        + (visible_rows as u16) * (1 + card_h)
        + (visible_rows as u16 - 1) * row_gap;
    let block_top = inner.y + content_h.saturating_sub(base) / 2;

    let mut row_tops = Vec::new();
    let mut bus_ys = Vec::new();
    for row in 0..visible_rows {
        let bus_y = block_top + forgejo_h + trunk_gap + row as u16 * (1 + card_h + row_gap);
        bus_ys.push(bus_y);
        row_tops.push(bus_y + 1);
    }

    let first_row = agent_count.min(columns);
    let trunk_x = choose_trunk_x(inner, first_row, card_w, gap);
    let spine_x = choose_spine_x(inner, columns, card_w, gap, trunk_x);

    GridGeometry {
        form: BoardForm::Cards,
        columns,
        first_visible,
        visible_rows,
        card_h,
        card_w,
        gap,
        forgejo_h,
        footer_rows,
        row_tops,
        bus_ys,
        spine_x,
        trunk_x,
        block_top,
        list: None,
        list_col_w: 0,
        list_x: 0,
    }
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

fn choose_spine_x(inner: Rect, columns: usize, card_w: u16, gap: u16, trunk_x: u16) -> u16 {
    if columns < 2 {
        return trunk_x;
    }
    let start = card_group_start(inner, columns, card_w, gap);
    let mut best = trunk_x;
    let mut best_distance = u16::MAX;
    for column in 0..columns.saturating_sub(1) {
        let gap_start = start + (column as u16 + 1) * card_w + column as u16 * gap;
        for offset in 0..gap {
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

/// Draw the bus for one row and update the spine continuation.
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
    if centres.is_empty() {
        return;
    }
    let left = centres[0];
    let right = *centres.last().unwrap_or(&left);
    let lo = spine_x.min(left);
    let hi = spine_x.max(right);
    let mut arms = vec![L | R; (hi - lo + 1) as usize];
    let index = |x: u16| (x - lo) as usize;

    arms[index(lo)] &= !L;
    arms[index(hi)] &= !R;

    // The spine's joint. It is the same cell as an end when there is one card.
    let at = &mut arms[index(spine_x)];
    if spine_from_above {
        *at |= U;
    }
    if spine_below {
        *at |= D;
    }

    for centre in centres {
        arms[index(*centre)] |= D;
    }

    if let Some(trunk_x) = trunk_x {
        if trunk_x >= lo && trunk_x <= hi {
            arms[index(trunk_x)] |= U;
        }
    }

    for (offset, arms) in arms.into_iter().enumerate() {
        put_arms(buf, clip, lo + offset as u16, bus_y, arms, style);
    }
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
fn draw_list_line(buf: &mut Buffer, rect: Rect, agent: &AgentView, selected: bool) {
    let face = status::agent_face(agent);
    let bg = if selected {
        theme::CARD_SELECTED
    } else {
        theme::CARD
    };
    text_util::fill(buf, rect, Style::new().fg(theme::FG).bg(bg));
    let max = rect.width as usize;
    let mut x = rect.x;
    let mut used = 0usize;
    let name_style = if selected {
        theme::card_bold(theme::SELECT, true)
    } else {
        theme::card_bold(theme::FG, true)
    };
    if selected && used < max {
        text_util::put(buf, rect, x, rect.y, "▸", name_style);
        x += 1;
        used += 1;
    }
    if used < max {
        text_util::put(
            buf,
            rect,
            x,
            rect.y,
            &face.symbol.to_string(),
            theme::card_bold(face.colour, selected),
        );
        x += 1;
        used += 1;
    }
    if used < max {
        text_util::put(buf, rect, x, rect.y, " ", name_style);
        x += 1;
        used += 1;
    }
    let name = text_util::ellipsize(&agent.name, max.saturating_sub(used));
    let name_width = text_util::width(&name);
    text_util::put(buf, rect, x, rect.y, &name, name_style);
    x += name_width as u16;
    used += name_width;
    let login = if agent.provisioning == Provisioning::Pending {
        "connecting…".to_owned()
    } else {
        agent.login.clone()
    };
    if used + 1 + text_util::width(&login) <= max {
        text_util::put(buf, rect, x, rect.y, " ", name_style);
        x += 1;
        used += 1;
        text_util::put(
            buf,
            rect,
            x,
            rect.y,
            &login,
            theme::card(theme::MUTED, selected),
        );
        x += text_util::width(&login) as u16;
        used += text_util::width(&login);
    }
    // The run total keeps its place here too: it is the number an earlier round
    // promised on every card (N5), and it never resets with the session.
    let count = status::run_count(agent);
    if used + 1 + text_util::width(&count) <= max {
        text_util::put(buf, rect, x, rect.y, " ", name_style);
        x += 1;
        used += 1;
        text_util::put(
            buf,
            rect,
            x,
            rect.y,
            &count,
            theme::card(theme::MUTED, selected),
        );
        x += text_util::width(&count) as u16;
        used += text_util::width(&count);
    }
    if used < max {
        text_util::put(buf, rect, x, rect.y, " ", name_style);
        x += 1;
        used += 1;
        let room = max.saturating_sub(used);
        // A state word is a word: draw it whole or leave it to the Agent panel.
        if text_util::width(&face.status) <= room {
            text_util::put(
                buf,
                rect,
                x,
                rect.y,
                &face.status,
                theme::card_bold(face.colour, selected),
            );
        }
    }
}

/// The one-line agent form: one wired box whose interior is a line per agent.
fn draw_agent_list(
    board: &Board,
    buf: &mut Buffer,
    geometry: &GridGeometry,
    forgejo_rect: Rect,
    wire_style: Style,
) -> BoardLayout {
    let list = geometry.list.expect("the list form has a box");
    for y in forgejo_rect.bottom()..list.y {
        put_arms(buf, buf.area, geometry.trunk_x, y, U | D, wire_style);
    }
    text_util::fill(buf, list, Style::new().fg(theme::FG).bg(theme::CARD));
    draw_plain_box(buf, list, Style::new().fg(theme::WIRE).bg(theme::CARD));
    // The trunk arrives at the middle of the box's top border as one `┴`.
    put_arms(
        buf,
        buf.area,
        geometry.trunk_x,
        list.y,
        L | R | U,
        wire_style,
    );

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
            draw_list_line(buf, rect, agent, selected);
            layout.listed.push(CardSlot {
                agent: agent_index,
                rect,
            });
        }
    }
    layout
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
    let forgejo_width = if roomy {
        let ceiling = if area.width >= 140 { 100 } else { 72 };
        (inner.width / 2 + 8).clamp(44, ceiling) & !1
    } else if area.width >= 80 {
        // 80 columns leaves room for two whole repository names beside the count;
        // the count and a `(+N)` still say what is hidden, never a cut name (F-09).
        (inner.width.saturating_sub(10)).clamp(50, 64) & !1
    } else {
        (inner.width.saturating_sub(16)).clamp(36, 50) & !1
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
        false,
    );

    let wire_style = theme::fg(theme::WIRE);
    let layout = if geometry.form == BoardForm::List {
        draw_agent_list(board, buf, &geometry, forgejo_rect, wire_style)
    } else {
        if geometry.columns > 1
            && let Some(first_bus) = geometry.bus_ys.first()
        {
            for y in forgejo_rect.bottom()..*first_bus {
                put_arms(buf, buf.area, geometry.trunk_x, y, U | D, wire_style);
            }
        }
        for (row, bus_y) in geometry.bus_ys.iter().enumerate() {
            if geometry.columns == 1 {
                break;
            }
            let index = geometry.first_visible + row * geometry.columns;
            let in_row = (agent_count.saturating_sub(index)).min(geometry.columns);
            if in_row == 0 {
                break;
            }
            let centres = row_junctions(inner, in_row, geometry.card_w, geometry.gap);
            draw_bus(
                buf,
                buf.area,
                *bus_y,
                &centres,
                geometry.spine_x,
                if row == 0 {
                    Some(geometry.trunk_x)
                } else {
                    None
                },
                row > 0,
                row + 1 < geometry.visible_rows,
                wire_style,
            );
            if row + 1 < geometry.visible_rows {
                draw_wrapped_row_spine(
                    buf,
                    buf.area,
                    geometry.spine_x,
                    *bus_y,
                    geometry.bus_ys[row + 1],
                    wire_style,
                );
            }
        }

        let mut layout = BoardLayout {
            columns: geometry.columns,
            first_visible: geometry.first_visible,
            cards: Vec::new(),
            listed: Vec::new(),
            form: BoardForm::Cards,
            forgejo: Some(forgejo_rect),
            wire_cells: Vec::new(),
            scrolls: agent_count > geometry.visible_rows * geometry.columns,
            footer_rows: geometry.footer_rows,
        };
        for row in 0..geometry.visible_rows {
            let index = geometry.first_visible + row * geometry.columns;
            let in_row = (agent_count.saturating_sub(index)).min(geometry.columns);
            if in_row == 0 {
                break;
            }
            let centres = row_junctions(inner, in_row, geometry.card_w, geometry.gap);
            let start = card_group_start(inner, in_row, geometry.card_w, geometry.gap);
            let y = geometry.row_tops[row];
            for (column, centre) in centres.iter().enumerate() {
                let agent_index = index + column;
                let Some(agent) = board.view.agents.get(agent_index) else {
                    continue;
                };
                let rect = Rect::new(
                    start + column as u16 * (geometry.card_w + geometry.gap),
                    y,
                    geometry.card_w,
                    geometry.card_h,
                );
                let selected = !board.forgejo_selected && agent_index == board.selection;
                if rect.height <= 2 {
                    draw_compact_card(buf, rect, agent, selected, true);
                } else {
                    draw_card(buf, rect, agent, selected, roomy, true);
                }
                if geometry.columns == 1 {
                    // One vertical list, one spine: the wire above this card leaves
                    // the Forgejo box (first card) or the previous card's bottom
                    // centre.
                    let junction_x = card_junction(rect);
                    if let Some(previous) = layout.cards.last() {
                        put_arms(
                            buf,
                            buf.area,
                            card_junction(previous.rect),
                            previous.rect.bottom() - 1,
                            L | R | D,
                            wire_style,
                        );
                    }
                    let from = layout
                        .cards
                        .last()
                        .map(|previous| previous.rect.bottom())
                        .unwrap_or(forgejo_rect.bottom());
                    for wire_y in from..rect.y {
                        put_arms(buf, buf.area, junction_x, wire_y, U | D, wire_style);
                    }
                }
                layout.cards.push(CardSlot {
                    agent: agent_index,
                    rect,
                });
                layout.wire_cells.push((*centre, y.saturating_sub(1)));
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
    let footer_rows = board.last.footer_rows.max(3);
    let footer_top = inner.bottom().saturating_sub(footer_rows);
    if board.confirm_quit {
        let started: Vec<&str> = board
            .view
            .agents
            .iter()
            .filter(|agent| agent.desired == Desired::Started)
            .map(|agent| agent.name.as_str())
            .collect();
        let names = join_names(&started);
        let line = if started.len() == 1 {
            format!(
                "quit stops {names} and Forgejo; they start again when you reopen — q again quits"
            )
        } else {
            format!(
                "quit stops {names}, and Forgejo; they start again when you reopen — q again quits"
            )
        };
        let width = inner.width.saturating_sub(2) as usize;
        for (y, part) in (footer_top..).zip(text_util::wrap(&line, width).into_iter().take(2)) {
            text_util::put(buf, inner, inner.x + 1, y, &part, theme::bold(theme::AMBER));
        }
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
    let bottom = inner.bottom().saturating_sub(1);
    // B3: the event owns a row. It is never right-aligned onto the key line, so a
    // long repository name or the scroll indicator cannot draw over a key hint.
    let event_y = if event.is_some() && footer_rows >= 4 {
        footer_top + 2
    } else {
        bottom
    };
    let status_y = bottom;
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
    // The status line has the bottom row unless the event took it (which the
    // geometry only allows when there is no notice, no waiting sentence and a
    // confined host).
    if event.is_none() || event_y != status_y {
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
        let line = format!("runs on this computer are confined ({method})");
        let lines = text_util::wrap(&line, width);
        if let Some((count, block)) = blocks.get_mut(1) {
            block.extend(lines);
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
        ((content_h as usize).saturating_sub(total_lines) / (blocks.len() - 1)).clamp(0, 4)
    } else {
        1
    };
    let block_h = total_lines + spacing * blocks.len().saturating_sub(1);
    let mut y = content_top + content_h.saturating_sub(block_h as u16) / 2;
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
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        "y agree    n leave",
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
    draw_outer(buf, area, "Your model");
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content_h = content_bottom.saturating_sub(content_top);
    let form_width = inner.width.saturating_sub(8).clamp(40, 76).min(inner.width);
    let field_gap = if content_h >= 22 { 3 } else { 1 };
    // The test line gets real rows, not one: the provider's whole answer wraps to
    // what the form can hold, and at 80x24 that is the whole refusal (F-07).
    let form_height = (9 + 2 * field_gap + 2 + 6).min(content_h);
    let form = Rect::new(
        inner.x + inner.width.saturating_sub(form_width) / 2,
        content_top + content_h.saturating_sub(form_height) / 2,
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
                "for example https://model.example/v1/chat/completions",
                theme::fg(theme::MUTED),
            );
            y += 1;
        }
        if index + 1 < labels.len() {
            y += field_gap;
        }
    }
    let test = &board.view.endpoint.test;
    let (line, style) = match test {
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
        "⏎ next field   ↑↓ field   esc cancel"
    } else {
        "e edit   ↑↓ field   t test   ⏎ next   esc back"
    };
    text_util::put(
        buf,
        inner,
        inner.x + 1,
        inner.bottom().saturating_sub(2),
        actions,
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
        if raw.is_empty() {
            if board.view.endpoint.key_present {
                "••••••••".to_owned()
            } else {
                "—".to_owned()
            }
        } else {
            "•".repeat(raw.chars().count().min(32))
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
    let block_top = content_top + content_h.saturating_sub(block) / 2;
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
    let spine_x = choose_spine_x(inner, columns, card_w, preview_gap, trunk_x);
    let forgejo_x = trunk_x
        .saturating_sub(forgejo_w / 2)
        .clamp(inner.x, inner.right().saturating_sub(forgejo_w));
    let forgejo = Rect::new(forgejo_x, block_top, forgejo_w, forgejo_h);
    draw_forgejo_box(buf, forgejo, &board.view.forgejo, count, false, true, true);
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
    draw_outer(buf, area, &agent.name);
    let inner = inner(area);
    let content_top = inner.y;
    let content_bottom = inner.bottom().saturating_sub(3);
    let content_h = content_bottom.saturating_sub(content_top);
    let card_width = inner.width.saturating_sub(8).clamp(40, 90).min(inner.width);
    // A tall terminal gives the instruction the spare rows instead of a fixed
    // 14-row card with the rest of the screen empty (U4/F-11).
    let card_height = content_h.saturating_sub(2).clamp(10, 44);
    let card = Rect::new(
        inner.x + inner.width.saturating_sub(card_width) / 2,
        content_top + content_h.saturating_sub(card_height) / 2,
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
    // rows at the top of the smallest card.
    let strip_y = card.bottom().saturating_sub(3);
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
    let failure_mark = status::failure_mark(&agent);
    let failure = agent
        .last_failure
        .as_ref()
        .map(|failure| format!("{failure_mark} {}", status::tool_failure(failure)))
        .or_else(|| {
            agent
                .last_error
                .as_ref()
                .map(|error| format!("✗ {}", status::first_clause(error)))
        });
    let result = status::run_result_line(&agent);
    let result_colour = if status::run_result_is_refusal(&agent) {
        theme::RED
    } else if status::run_result_is_warning(&agent) {
        theme::AMBER
    } else {
        theme::MUTED
    };
    // The failure and the run's own result answer different questions, and both are
    // drawn: a transient failed tool must not hide the summary beside `✓` (review
    // B2). The activity keeps the row under the status, then the failure, then the
    // result.
    let mut extras: Vec<(String, ratatui::style::Style)> = Vec::new();
    if let Some(failure) = failure {
        extras.push((failure, theme::card(theme::RED, true)));
    }
    if let Some(result) = result {
        extras.push((result, theme::card(result_colour, true)));
    }
    let (second, extras) = match (&activity, extras.is_empty()) {
        (Some(activity), _) => (
            Some((activity.clone(), theme::card(theme::MUTED, true))),
            extras,
        ),
        (None, false) => {
            let first = extras.remove(0);
            (Some(first), extras)
        }
        (None, true) => (None, extras),
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
    let extra_rows = extras.len() as u16;
    for (index, (notice, style)) in extras.iter().enumerate() {
        text_util::put(
            buf,
            card,
            interior_x,
            card.y + 3 + index as u16,
            &text_util::ellipsize_words(notice, width),
            *style,
        );
    }

    // The work block: the last issue / pull request / branch / comment a work run
    // named, correctly typed and labelled `work`, in at most three lines (F5). It is
    // kept until a later run does new work, so a read-only check run does not erase it
    // (W31-3). With no work at all, the Agent screen names the assignment rule.
    let work_start = card.y + 3 + extra_rows;
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
    text_util::put(
        buf,
        card,
        interior_x,
        label_y,
        "instruction",
        theme::card(theme::MUTED, true),
    );
    match (&agent.instruction, &agent.instruction_first_line) {
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
    if card.bottom().saturating_sub(1) > strip_y + 1 {
        // One space between the words keeps the whole legend inside the 72-column
        // card's 58-cell row. `■ stop` is the floor spelling; from 80 columns the
        // card has 66 cells and the whole `■ stopped` fits. The `⚠ cmd` entry is the
        // new mark's: a finished run with a failed command (correction 4).
        // The 72-column card has 58 cells: this spelling fits whole, with every
        // entry's word complete. `∅ none` is the no-work mark; the longer `no work`
        // is used where the card has room (F-04).
        let legend = if width >= 61 {
            "✓ ok ⊘ refused ∅ no work ✗ failed ⚠ cmd ■ stopped ◷ timed out"
        } else {
            "✓ ok ⊘ refused ∅ none ✗ failed ⚠ cmd ■ stopped ◷ timed out"
        };
        text_util::put(
            buf,
            card,
            interior_x,
            strip_y + 1,
            &text_util::ellipsize(legend, width),
            theme::card(theme::MUTED, true),
        );
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
    } else if text_util::width(
        "s start/stop   e instruction   l log   t transcript   d remove   esc back",
    ) <= inner.width.saturating_sub(2) as usize
    {
        let line = "s start/stop   e instruction   l log   t transcript   d remove   esc back";
        text_util::put(
            buf,
            inner,
            inner.x + 1,
            actions_y,
            line,
            theme::fg(theme::MUTED),
        );
    } else {
        let line1 = "s start/stop   e instruction   l log";
        let line2 = "t transcript   d remove   esc back";
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
        for line in physical {
            for part in line {
                text_util::put(
                    buf,
                    clip,
                    x,
                    top + row as u16,
                    &text_util::ellipsize_words(part, width),
                    theme::card(theme::FG, true),
                );
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
            text_util::put(
                buf,
                clip,
                x,
                top + drawn as u16,
                &text_util::ellipsize_words(part, width),
                theme::card(theme::FG, true),
            );
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
        ("organisation", vec![view.organisation.clone()]),
        ("repositories", text_util::wrap(&repositories, room)),
        ("your login:", text_util::wrap(&login, room)),
        ("password", text_util::wrap(&password, room)),
        (
            "agents can",
            vec!["write to team repositories only".to_owned()],
        ),
        (
            "agents work",
            text_util::wrap(
                "only on issues assigned to them — assign an issue in Forgejo",
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
    let mut y = content_top + content_h.saturating_sub(block as u16) / 2;
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
    let mut y = content_top + rule.saturating_sub(total) as u16 / 2;
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
        ("", "t transcript   d remove   esc back"),
        ("model", "e edit   t test   ⏎ next   esc back"),
        ("Forgejo", "p password   esc back"),
        ("first run", "y agree   n leave"),
    ];
    let line_count = lines.len() as u16;
    let spacing = if line_count > 1 {
        (content_h.saturating_sub(line_count) / (line_count - 1)).clamp(0, 2)
    } else {
        0
    };
    let block = line_count + spacing * line_count.saturating_sub(1);
    let mut y = content_top + content_h.saturating_sub(block) / 2;
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
