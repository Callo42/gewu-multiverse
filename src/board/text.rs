//! Small text helpers for the board.
//!
//! The board draws fixed cells itself, so it needs to know how wide a string is,
//! where to cut it, and how to wrap a line without ever handing half a character to
//! a cell. No dependency is added for this: the board's glyphs are one cell wide and
//! a small table covers the wide characters a person's instruction may contain.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// How many terminal cells one character occupies.
///
/// This is the width table the board relies on for clipping and wrapping. It covers
/// the wide blocks a person may paste into an instruction; everything else is one
/// cell. Combining marks are zero cells so they never overwrite a neighbouring cell.
pub fn char_width(c: char) -> usize {
    let code = c as u32;
    if c.is_control() {
        return 0;
    }
    match code {
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F => {
            0
        }
        0x1100..=0x115F
        | 0x2329..=0x232A
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// The width of a string in terminal cells.
pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Write a string at a cell, clipped to a rectangle.
///
/// A wide character's second cell is painted too, so nothing behind it shows through
/// and no cell is left on the terminal's default background.
pub fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < clip.y || y >= clip.bottom() {
        return;
    }
    let mut cx = x;
    let mut bytes = [0_u8; 4];
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            break;
        }
        let w = char_width(ch);
        if w == 0 {
            continue;
        }
        if cx >= clip.right() || cx.saturating_add(w as u16) > clip.right() {
            break;
        }
        if cx >= clip.x {
            let symbol = ch.encode_utf8(&mut bytes);
            buf[(cx, y)].set_symbol(symbol).set_style(style);
            for extra in 1..w {
                let at = cx + extra as u16;
                if at < clip.right() {
                    buf[(at, y)].set_symbol(" ").set_style(style);
                }
            }
        }
        cx = cx.saturating_add(w as u16);
    }
}

/// Fill a rectangle with spaces in one style.
pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_symbol(" ").set_style(style);
        }
    }
}

/// A string cut to at most `max` cells, with an ellipsis when something was cut.
pub fn ellipsize(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w + 1 > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// A string cut at a whole word, with an ellipsis when something was cut.
///
/// A card line is a sentence to a reader: `… push th…` reads as a typo, so the cut
/// falls back to the last space. A single word longer than the line still has to be
/// split; that is the one case `ellipsize` handles.
pub fn ellipsize_words(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w + 1 > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    match out.rfind(' ') {
        Some(at) if !out[..at].trim().is_empty() => {
            format!("{}…", out[..at].trim_end())
        }
        _ => {
            out.push('…');
            out
        }
    }
}

/// Wrap a string into lines of at most `max` cells.
///
/// Explicit newlines are honoured. A word longer than the line is split rather than
/// allowed to overflow; a wrapped line never carries a partial character.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split_whitespace() {
            let word_width = width(word);
            if used > 0 && used + 1 + word_width > max {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if word_width > max {
                if used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                let mut piece = String::new();
                for ch in word.chars() {
                    let w = char_width(ch);
                    if used + w > max {
                        lines.push(std::mem::take(&mut piece));
                        used = 0;
                    }
                    piece.push(ch);
                    used += w;
                }
                line = piece;
                used = width(&line);
                continue;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += word_width;
        }
        if !line.is_empty() || paragraph.is_empty() {
            lines.push(line);
        }
    }
    lines
}

/// How many whitespace-separated words a string holds.
pub fn words(text: &str) -> usize {
    text.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_character_takes_two_cells() {
        assert_eq!(width("ab"), 2);
        assert_eq!(width("界"), 2);
        assert_eq!(width("a界b"), 4);
    }

    #[test]
    fn ellipsis_never_overflows_and_says_it_cut() {
        assert_eq!(ellipsize("abcdef", 6), "abcdef");
        assert_eq!(ellipsize("abcdef", 5), "abcd…");
        assert_eq!(width(&ellipsize("abcdef", 5)), 5);
        assert_eq!(ellipsize("abcdef", 0), "");
    }

    #[test]
    fn a_word_cut_does_not_split_the_word() {
        assert_eq!(ellipsize_words("push the branch", 11), "push the…");
        assert_eq!(ellipsize_words("push the branch", 80), "push the branch");
        // One word longer than the line still has to be split; there is no space.
        assert_eq!(ellipsize_words("abcdefghij", 4), "abc…");
    }

    #[test]
    fn wrapping_keeps_every_word_and_the_width() {
        let lines = wrap("one two three four", 7);
        assert_eq!(lines, ["one two", "three", "four"]);
        assert!(lines.iter().all(|line| width(line) <= 7));
        let lines = wrap("abcdefghij", 4);
        assert_eq!(lines, ["abcd", "efgh", "ij"]);
    }
}
