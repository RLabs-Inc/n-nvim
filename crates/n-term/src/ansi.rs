// SPDX-License-Identifier: MIT
//
// ANSI escape sequence generation.
//
// Pure functions that write escape sequences to any `impl Write`. No state,
// no decisions about when to emit — that's the `CellWriter`'s job. This module
// just knows the byte-level encoding of every terminal command we need.
//
// All cursor positions are 0-indexed in our API and converted to 1-indexed
// for the terminal (ANSI standard uses 1-based coordinates).
//
// All functions return `io::Result` propagated from the underlying writer.
// In practice they never fail when writing to `OutputBuffer` (backed by a Vec).
use std::io::{self, Write};

use crate::cell::{Attr, UnderlineStyle};
use crate::color::CellColor;

// ─── Cursor ──────────────────────────────────────────────────────────────────

/// Move the cursor to `(x, y)` using the CUP (Cursor Position) sequence.
///
/// Our coordinates are 0-indexed; ANSI CUP is 1-indexed.
#[inline]
pub fn cursor_to(w: &mut impl Write, x: u16, y: u16) -> io::Result<()> {
    write!(w, "\x1b[{};{}H", y + 1, x + 1)
}

/// Hide the cursor (DECTCEM reset).
#[inline]
pub fn cursor_hide(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?25l")
}

/// Show the cursor (DECTCEM set).
#[inline]
pub fn cursor_show(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?25h")
}

// ─── Screen ──────────────────────────────────────────────────────────────────

/// Clear the entire screen (ED 2).
#[inline]
pub fn clear_screen(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[2J")
}

/// Reset all SGR attributes to terminal defaults (SGR 0).
///
/// This clears **everything**: bold, italic, colors, underline — all of it.
/// The stateful renderer must invalidate its tracked state after calling this.
#[inline]
pub fn reset(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[0m")
}

// ─── Foreground Color ────────────────────────────────────────────────────────

/// Set the foreground (text) color.
///
/// Uses compact SGR codes for standard colors (30-37, 90-97), the 256-color
/// extended format for palette indices 16-255, and 24-bit `TrueColor` for RGB.
pub fn fg(w: &mut impl Write, color: CellColor) -> io::Result<()> {
    match color {
        CellColor::Default => w.write_all(b"\x1b[39m"),
        CellColor::Ansi256(idx) => {
            if idx < 8 {
                write!(w, "\x1b[{}m", 30 + u16::from(idx))
            } else if idx < 16 {
                write!(w, "\x1b[{}m", 82 + u16::from(idx))
            } else {
                write!(w, "\x1b[38;5;{idx}m")
            }
        }
        CellColor::Rgb(r, g, b) => write!(w, "\x1b[38;2;{r};{g};{b}m"),
    }
}

// ─── Background Color ────────────────────────────────────────────────────────

/// Set the background color.
///
/// Same encoding strategy as [`fg`] but with BG-specific SGR codes
/// (40–47, 100–107, 48;5;N, 48;2;R;G;B).
pub fn bg(w: &mut impl Write, color: CellColor) -> io::Result<()> {
    match color {
        CellColor::Default => w.write_all(b"\x1b[49m"),
        CellColor::Ansi256(idx) => {
            if idx < 8 {
                write!(w, "\x1b[{}m", 40 + u16::from(idx))
            } else if idx < 16 {
                write!(w, "\x1b[{}m", 92 + u16::from(idx))
            } else {
                write!(w, "\x1b[48;5;{idx}m")
            }
        }
        CellColor::Rgb(r, g, b) => write!(w, "\x1b[48;2;{r};{g};{b}m"),
    }
}

// ─── Text Attributes ─────────────────────────────────────────────────────────

/// Emit SGR codes for text attributes as a single CSI sequence.
///
/// Multiple attributes are semicolon-separated: `\x1b[1;3;9m` for
/// bold + italic + strikethrough. Does nothing if no attributes are set.
pub fn attrs(w: &mut impl Write, attr: Attr) -> io::Result<()> {
    if attr.is_empty() {
        return Ok(());
    }

    w.write_all(b"\x1b[")?;
    let mut first = true;

    macro_rules! emit {
        ($flag:expr, $code:expr) => {
            if attr.contains($flag) {
                if !first {
                    w.write_all(b";")?;
                }
                w.write_all($code)?;
                first = false;
            }
        };
    }

    emit!(Attr::BOLD, b"1");
    emit!(Attr::DIM, b"2");
    emit!(Attr::ITALIC, b"3");
    emit!(Attr::SLOW_BLINK, b"5");
    emit!(Attr::RAPID_BLINK, b"6");
    emit!(Attr::INVERSE, b"7");
    emit!(Attr::HIDDEN, b"8");
    emit!(Attr::STRIKETHROUGH, b"9");
    let _ = first; // Last expansion sets first; suppress dead-write warning.

    w.write_all(b"m")
}

// ─── Underline Style ─────────────────────────────────────────────────────────

/// Set the underline style using modern SGR 4:N colon syntax.
///
/// Modern terminals (Kitty, `WezTerm`, Ghostty, iTerm2) support the colon
/// sub-parameter syntax for underline variants. `None` disables underline
/// via SGR 24 (underline off).
pub fn underline(w: &mut impl Write, style: UnderlineStyle) -> io::Result<()> {
    match style {
        UnderlineStyle::None => w.write_all(b"\x1b[24m"),
        UnderlineStyle::Straight => w.write_all(b"\x1b[4:1m"),
        UnderlineStyle::Double => w.write_all(b"\x1b[4:2m"),
        UnderlineStyle::Curly => w.write_all(b"\x1b[4:3m"),
        UnderlineStyle::Dotted => w.write_all(b"\x1b[4:4m"),
        UnderlineStyle::Dashed => w.write_all(b"\x1b[4:5m"),
    }
}

// ─── Synchronized Output ─────────────────────────────────────────────────────

/// Begin synchronized output (DEC Private Mode 2026).
///
/// Tells the terminal to buffer all subsequent output until [`end_sync`].
/// This prevents partial frame updates from causing visible flicker.
/// Supported by modern terminals: Kitty, `WezTerm`, iTerm2, foot, etc.
#[inline]
pub fn begin_sync(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?2026h")
}

/// End synchronized output — terminal renders the buffered frame.
#[inline]
pub fn end_sync(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?2026l")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: run an ANSI function and return its output as a string.
    fn emit<F>(f: F) -> String
    where
        F: FnOnce(&mut Vec<u8>) -> io::Result<()>,
    {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── Cursor ──────────────────────────────────────────────────────────

    #[test]
    fn cursor_to_origin() {
        assert_eq!(emit(|w| cursor_to(w, 0, 0)), "\x1b[1;1H");
    }

    #[test]
    fn cursor_to_position() {
        assert_eq!(emit(|w| cursor_to(w, 10, 20)), "\x1b[21;11H");
    }

    #[test]
    fn cursor_to_max() {
        // Verify no overflow with large coordinates.
        let s = emit(|w| cursor_to(w, 999, 499));
        assert_eq!(s, "\x1b[500;1000H");
    }

    #[test]
    fn cursor_hide_sequence() {
        assert_eq!(emit(|w| cursor_hide(w)), "\x1b[?25l");
    }

    #[test]
    fn cursor_show_sequence() {
        assert_eq!(emit(|w| cursor_show(w)), "\x1b[?25h");
    }

    // ── Screen ──────────────────────────────────────────────────────────

    #[test]
    fn clear_screen_sequence() {
        assert_eq!(emit(|w| clear_screen(w)), "\x1b[2J");
    }

    #[test]
    fn reset_sequence() {
        assert_eq!(emit(|w| reset(w)), "\x1b[0m");
    }

    // ── Foreground Color ────────────────────────────────────────────────

    #[test]
    fn fg_default() {
        assert_eq!(emit(|w| fg(w, CellColor::Default)), "\x1b[39m");
    }

    #[test]
    fn fg_ansi_black() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(0))), "\x1b[30m");
    }

    #[test]
    fn fg_ansi_standard_red() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(1))), "\x1b[31m");
    }

    #[test]
    fn fg_ansi_standard_white() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(7))), "\x1b[37m");
    }

    #[test]
    fn fg_ansi_bright_black() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(8))), "\x1b[90m");
    }

    #[test]
    fn fg_ansi_bright_red() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(9))), "\x1b[91m");
    }

    #[test]
    fn fg_ansi_bright_white() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(15))), "\x1b[97m");
    }

    #[test]
    fn fg_ansi_extended() {
        assert_eq!(emit(|w| fg(w, CellColor::Ansi256(42))), "\x1b[38;5;42m");
    }

    #[test]
    fn fg_ansi_extended_max() {
        assert_eq!(
            emit(|w| fg(w, CellColor::Ansi256(255))),
            "\x1b[38;5;255m"
        );
    }

    #[test]
    fn fg_rgb() {
        assert_eq!(
            emit(|w| fg(w, CellColor::Rgb(255, 128, 0))),
            "\x1b[38;2;255;128;0m"
        );
    }

    #[test]
    fn fg_rgb_black() {
        assert_eq!(
            emit(|w| fg(w, CellColor::Rgb(0, 0, 0))),
            "\x1b[38;2;0;0;0m"
        );
    }

    // ── Background Color ────────────────────────────────────────────────

    #[test]
    fn bg_default() {
        assert_eq!(emit(|w| bg(w, CellColor::Default)), "\x1b[49m");
    }

    #[test]
    fn bg_ansi_standard_green() {
        assert_eq!(emit(|w| bg(w, CellColor::Ansi256(2))), "\x1b[42m");
    }

    #[test]
    fn bg_ansi_standard_white() {
        assert_eq!(emit(|w| bg(w, CellColor::Ansi256(7))), "\x1b[47m");
    }

    #[test]
    fn bg_ansi_bright_black() {
        assert_eq!(emit(|w| bg(w, CellColor::Ansi256(8))), "\x1b[100m");
    }

    #[test]
    fn bg_ansi_bright_green() {
        assert_eq!(emit(|w| bg(w, CellColor::Ansi256(10))), "\x1b[102m");
    }

    #[test]
    fn bg_ansi_bright_white() {
        assert_eq!(emit(|w| bg(w, CellColor::Ansi256(15))), "\x1b[107m");
    }

    #[test]
    fn bg_ansi_extended() {
        assert_eq!(
            emit(|w| bg(w, CellColor::Ansi256(200))),
            "\x1b[48;5;200m"
        );
    }

    #[test]
    fn bg_rgb() {
        assert_eq!(
            emit(|w| bg(w, CellColor::Rgb(0, 100, 200))),
            "\x1b[48;2;0;100;200m"
        );
    }

    // ── Text Attributes ─────────────────────────────────────────────────

    #[test]
    fn attrs_empty_emits_nothing() {
        assert_eq!(emit(|w| attrs(w, Attr::empty())), "");
    }

    #[test]
    fn attrs_bold() {
        assert_eq!(emit(|w| attrs(w, Attr::BOLD)), "\x1b[1m");
    }

    #[test]
    fn attrs_dim() {
        assert_eq!(emit(|w| attrs(w, Attr::DIM)), "\x1b[2m");
    }

    #[test]
    fn attrs_italic() {
        assert_eq!(emit(|w| attrs(w, Attr::ITALIC)), "\x1b[3m");
    }

    #[test]
    fn attrs_strikethrough() {
        assert_eq!(emit(|w| attrs(w, Attr::STRIKETHROUGH)), "\x1b[9m");
    }

    #[test]
    fn attrs_combined_bold_italic() {
        assert_eq!(
            emit(|w| attrs(w, Attr::BOLD | Attr::ITALIC)),
            "\x1b[1;3m"
        );
    }

    #[test]
    fn attrs_combined_three() {
        let style = Attr::BOLD | Attr::ITALIC | Attr::STRIKETHROUGH;
        assert_eq!(emit(|w| attrs(w, style)), "\x1b[1;3;9m");
    }

    #[test]
    fn attrs_all() {
        let all = Attr::BOLD
            | Attr::DIM
            | Attr::ITALIC
            | Attr::SLOW_BLINK
            | Attr::RAPID_BLINK
            | Attr::INVERSE
            | Attr::HIDDEN
            | Attr::STRIKETHROUGH;
        assert_eq!(emit(|w| attrs(w, all)), "\x1b[1;2;3;5;6;7;8;9m");
    }

    // ── Underline Style ─────────────────────────────────────────────────

    #[test]
    fn underline_none_disables() {
        assert_eq!(emit(|w| underline(w, UnderlineStyle::None)), "\x1b[24m");
    }

    #[test]
    fn underline_straight() {
        assert_eq!(
            emit(|w| underline(w, UnderlineStyle::Straight)),
            "\x1b[4:1m"
        );
    }

    #[test]
    fn underline_double() {
        assert_eq!(
            emit(|w| underline(w, UnderlineStyle::Double)),
            "\x1b[4:2m"
        );
    }

    #[test]
    fn underline_curly() {
        assert_eq!(
            emit(|w| underline(w, UnderlineStyle::Curly)),
            "\x1b[4:3m"
        );
    }

    #[test]
    fn underline_dotted() {
        assert_eq!(
            emit(|w| underline(w, UnderlineStyle::Dotted)),
            "\x1b[4:4m"
        );
    }

    #[test]
    fn underline_dashed() {
        assert_eq!(
            emit(|w| underline(w, UnderlineStyle::Dashed)),
            "\x1b[4:5m"
        );
    }

    // ── Synchronized Output ─────────────────────────────────────────────

    #[test]
    fn sync_begin() {
        assert_eq!(emit(|w| begin_sync(w)), "\x1b[?2026h");
    }

    #[test]
    fn sync_end() {
        assert_eq!(emit(|w| end_sync(w)), "\x1b[?2026l");
    }

    // ── Composition ─────────────────────────────────────────────────────

    #[test]
    fn multiple_sequences_compose() {
        let mut buf = Vec::new();
        cursor_to(&mut buf, 5, 3).unwrap();
        fg(&mut buf, CellColor::Rgb(255, 0, 0)).unwrap();
        bg(&mut buf, CellColor::Ansi256(0)).unwrap();
        attrs(&mut buf, Attr::BOLD).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "\x1b[4;6H\x1b[38;2;255;0;0m\x1b[40m\x1b[1m");
    }
}
