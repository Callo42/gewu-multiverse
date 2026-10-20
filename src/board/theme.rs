//! The board's colours and text styles.
//!
//! Every style names a background. The board paints its own background on every cell
//! before anything else, so a terminal's theme can never show through and the
//! foreground/background pairs stay the ones the design measured.

use ratatui::style::{Color, Modifier, Style};

/// The product's own background: painted on every cell, never left to the terminal.
pub const BG: Color = Color::Rgb(0, 0, 0);
/// Ordinary body text.
pub const FG: Color = Color::Rgb(232, 232, 232);
/// Secondary text: addresses, logins, counts.
pub const MUTED: Color = Color::Rgb(148, 163, 184);
/// The Forgejo accent.
pub const ACCENT: Color = Color::Rgb(251, 146, 60);
/// A running agent or a running Forgejo.
pub const RUNNING: Color = Color::Rgb(74, 222, 128);
/// Restarting, fetching, preparing, starting.
pub const AMBER: Color = Color::Rgb(251, 191, 36);
/// Blocked, failed, and other refusals.
pub const RED: Color = Color::Rgb(248, 113, 113);
/// Stopped, missing, pending.
pub const GREY: Color = Color::Rgb(148, 163, 184);
/// A selected card or box.
pub const SELECT: Color = Color::Rgb(34, 211, 238);
/// A card's own background.
pub const CARD: Color = Color::Rgb(15, 23, 42);
/// The selected card's background.
pub const CARD_SELECTED: Color = Color::Rgb(8, 51, 68);
/// A wire between the Forgejo box and a card.
pub const WIRE: Color = Color::Rgb(71, 85, 105);
/// A field the person is writing into.
pub const FIELD: Color = Color::Rgb(30, 41, 59);

/// Body text on the product background.
pub fn base() -> Style {
    Style::new().fg(FG).bg(BG)
}

/// Text in one colour on the product background.
pub fn fg(color: Color) -> Style {
    Style::new().fg(color).bg(BG)
}

/// Text in one colour, bold, on the product background.
pub fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

/// Text on a card's background.
pub fn card(color: Color, selected: bool) -> Style {
    Style::new()
        .fg(color)
        .bg(if selected { CARD_SELECTED } else { CARD })
}

/// Text on the selected card, bold.
pub fn card_bold(color: Color, selected: bool) -> Style {
    card(color, selected).add_modifier(Modifier::BOLD)
}

/// A field's text, on the field background.
pub fn field(color: Color) -> Style {
    Style::new().fg(color).bg(FIELD)
}

/// The field the keyboard is on: a bright background, not only a coloured label.
pub fn focus_field() -> Style {
    Style::new().fg(BG).bg(SELECT).add_modifier(Modifier::BOLD)
}
