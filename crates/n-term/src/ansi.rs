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

// ─── Alternate Screen ───────────────────────────────────────────────────────

/// Enter the alternate screen buffer (DEC Private Mode 1049).
///
/// The alternate screen is a separate buffer that preserves the original
/// terminal content. On exit, the original content is restored — this is
/// what makes TUI applications non-destructive.
#[inline]
pub fn enter_alt_screen(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?1049h")
}

/// Exit the alternate screen buffer and restore original content.
#[inline]
pub fn exit_alt_screen(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?1049l")
}

// ─── Mouse Protocol ─────────────────────────────────────────────────────────

/// Mouse tracking granularity for SGR mouse protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Report button press and release events (DEC 1000).
    Click,
    /// Report button events and drag motion (DEC 1000 + 1002).
    Drag,
    /// Report all mouse motion, even without buttons held (DEC 1000 + 1002 + 1003).
    Motion,
}

/// Enable SGR mouse tracking at the specified granularity.
///
/// Uses SGR format (DEC 1006) which supports coordinates beyond column 223
/// and distinguishes button press from release. Call [`disable_mouse`] before
/// changing modes to avoid stale tracking flags.
pub fn enable_mouse(w: &mut impl Write, mode: MouseMode) -> io::Result<()> {
    w.write_all(b"\x1b[?1000h")?;
    if matches!(mode, MouseMode::Drag | MouseMode::Motion) {
        w.write_all(b"\x1b[?1002h")?;
    }
    if mode == MouseMode::Motion {
        w.write_all(b"\x1b[?1003h")?;
    }
    w.write_all(b"\x1b[?1006h")
}

/// Disable all mouse tracking.
pub fn disable_mouse(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?1006l")?;
    w.write_all(b"\x1b[?1003l")?;
    w.write_all(b"\x1b[?1002l")?;
    w.write_all(b"\x1b[?1000l")
}

// ─── Kitty Keyboard Protocol ────────────────────────────────────────────────

/// Enable the Kitty keyboard protocol with progressive enhancement flags.
///
/// Flags (bitfield, combine with `|`):
/// - `1` — Disambiguate escape codes (essential for editors).
/// - `2` — Report event types (press / release / repeat).
/// - `4` — Report alternate keys.
/// - `8` — Report all keys as escape codes.
/// - `16` — Report associated text.
///
/// For a text editor, `1` (disambiguate) is the minimum useful flag.
#[inline]
pub fn enable_kitty_keyboard(w: &mut impl Write, flags: u8) -> io::Result<()> {
    write!(w, "\x1b[>{flags}u")
}

/// Disable the Kitty keyboard protocol (pop enhancement from stack).
#[inline]
pub fn disable_kitty_keyboard(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[<u")
}

// ─── Bracketed Paste ────────────────────────────────────────────────────────

/// Enable bracketed paste mode (DEC 2004).
///
/// Pasted text is wrapped with `\x1b[200~` / `\x1b[201~`, letting the
/// editor distinguish typed input from clipboard paste. Without this,
/// pasting triggers auto-indent on every line.
#[inline]
pub fn enable_bracketed_paste(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?2004h")
}

/// Disable bracketed paste mode.
#[inline]
pub fn disable_bracketed_paste(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?2004l")
}

// ─── Focus Reporting ────────────────────────────────────────────────────────

/// Enable terminal focus reporting (DEC 1004).
///
/// The terminal sends `\x1b[I` on focus gain and `\x1b[O` on focus loss.
/// Useful for pausing animations, dimming the UI, or refreshing file state
/// when the user returns from another window.
#[inline]
pub fn enable_focus_reporting(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?1004h")
}

/// Disable terminal focus reporting.
#[inline]
pub fn disable_focus_reporting(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b[?1004l")
}

// ─── Cursor Shape ───────────────────────────────────────────────────────────

/// Terminal cursor shape (DECSCUSR — Set Cursor Style).
///
/// Used for Vim mode indication:
/// - Normal mode → [`SteadyBlock`](CursorShape::SteadyBlock)
/// - Insert mode → [`SteadyBar`](CursorShape::SteadyBar)
/// - Replace mode → [`SteadyUnderline`](CursorShape::SteadyUnderline)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// Terminal default (usually blinking block).
    #[default]
    Default,
    /// Blinking block cursor.
    BlinkBlock,
    /// Steady (non-blinking) block cursor.
    SteadyBlock,
    /// Blinking underline cursor.
    BlinkUnderline,
    /// Steady underline cursor.
    SteadyUnderline,
    /// Blinking bar (I-beam) cursor.
    BlinkBar,
    /// Steady bar (I-beam) cursor.
    SteadyBar,
}

/// Set the cursor shape using DECSCUSR.
///
/// Supported by all modern terminals: Kitty, `WezTerm`, Ghostty, iTerm2,
/// Alacritty, foot, Windows Terminal.
#[inline]
pub fn set_cursor_shape(w: &mut impl Write, shape: CursorShape) -> io::Result<()> {
    let n: u8 = match shape {
        CursorShape::Default => 0,
        CursorShape::BlinkBlock => 1,
        CursorShape::SteadyBlock => 2,
        CursorShape::BlinkUnderline => 3,
        CursorShape::SteadyUnderline => 4,
        CursorShape::BlinkBar => 5,
        CursorShape::SteadyBar => 6,
    };
    write!(w, "\x1b[{n} q")
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

    // ── Alternate Screen ────────────────────────────────────────────────

    #[test]
    fn enter_alt_screen_sequence() {
        assert_eq!(emit(|w| enter_alt_screen(w)), "\x1b[?1049h");
    }

    #[test]
    fn exit_alt_screen_sequence() {
        assert_eq!(emit(|w| exit_alt_screen(w)), "\x1b[?1049l");
    }

    // ── Mouse Protocol ──────────────────────────────────────────────────

    #[test]
    fn enable_mouse_click_mode() {
        let output = emit(|w| enable_mouse(w, MouseMode::Click));
        assert!(output.contains("\x1b[?1000h"));
        assert!(output.contains("\x1b[?1006h"));
        assert!(!output.contains("\x1b[?1002h"));
        assert!(!output.contains("\x1b[?1003h"));
    }

    #[test]
    fn enable_mouse_drag_mode() {
        let output = emit(|w| enable_mouse(w, MouseMode::Drag));
        assert!(output.contains("\x1b[?1000h"));
        assert!(output.contains("\x1b[?1002h"));
        assert!(output.contains("\x1b[?1006h"));
        assert!(!output.contains("\x1b[?1003h"));
    }

    #[test]
    fn enable_mouse_motion_mode() {
        let output = emit(|w| enable_mouse(w, MouseMode::Motion));
        assert!(output.contains("\x1b[?1000h"));
        assert!(output.contains("\x1b[?1002h"));
        assert!(output.contains("\x1b[?1003h"));
        assert!(output.contains("\x1b[?1006h"));
    }

    #[test]
    fn disable_mouse_all_modes() {
        let output = emit(|w| disable_mouse(w));
        assert!(output.contains("\x1b[?1006l"));
        assert!(output.contains("\x1b[?1003l"));
        assert!(output.contains("\x1b[?1002l"));
        assert!(output.contains("\x1b[?1000l"));
    }

    // ── Kitty Keyboard Protocol ─────────────────────────────────────────

    #[test]
    fn enable_kitty_keyboard_disambiguate() {
        assert_eq!(emit(|w| enable_kitty_keyboard(w, 1)), "\x1b[>1u");
    }

    #[test]
    fn enable_kitty_keyboard_all_flags() {
        assert_eq!(emit(|w| enable_kitty_keyboard(w, 31)), "\x1b[>31u");
    }

    #[test]
    fn disable_kitty_keyboard_sequence() {
        assert_eq!(emit(|w| disable_kitty_keyboard(w)), "\x1b[<u");
    }

    // ── Bracketed Paste ─────────────────────────────────────────────────

    #[test]
    fn enable_bracketed_paste_sequence() {
        assert_eq!(emit(|w| enable_bracketed_paste(w)), "\x1b[?2004h");
    }

    #[test]
    fn disable_bracketed_paste_sequence() {
        assert_eq!(emit(|w| disable_bracketed_paste(w)), "\x1b[?2004l");
    }

    // ── Focus Reporting ─────────────────────────────────────────────────

    #[test]
    fn enable_focus_reporting_sequence() {
        assert_eq!(emit(|w| enable_focus_reporting(w)), "\x1b[?1004h");
    }

    #[test]
    fn disable_focus_reporting_sequence() {
        assert_eq!(emit(|w| disable_focus_reporting(w)), "\x1b[?1004l");
    }

    // ── Cursor Shape ────────────────────────────────────────────────────

    #[test]
    fn cursor_shape_default() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::Default)),
            "\x1b[0 q"
        );
    }

    #[test]
    fn cursor_shape_blink_block() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::BlinkBlock)),
            "\x1b[1 q"
        );
    }

    #[test]
    fn cursor_shape_steady_block() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::SteadyBlock)),
            "\x1b[2 q"
        );
    }

    #[test]
    fn cursor_shape_blink_underline() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::BlinkUnderline)),
            "\x1b[3 q"
        );
    }

    #[test]
    fn cursor_shape_steady_underline() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::SteadyUnderline)),
            "\x1b[4 q"
        );
    }

    #[test]
    fn cursor_shape_blink_bar() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::BlinkBar)),
            "\x1b[5 q"
        );
    }

    #[test]
    fn cursor_shape_steady_bar() {
        assert_eq!(
            emit(|w| set_cursor_shape(w, CursorShape::SteadyBar)),
            "\x1b[6 q"
        );
    }
}
