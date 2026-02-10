// SPDX-License-Identifier: MIT
//
// Output buffering and stateful cell rendering.
//
// Two components work together to minimize terminal I/O:
//
//   OutputBuffer — accumulates all ANSI bytes in memory so the entire frame
//   can be written in a single write() syscall. This eliminates per-escape
//   overhead and keeps the terminal's input parser happy.
//
//   CellWriter — tracks the terminal's current state (cursor position, colors,
//   attributes, underline style) and skips redundant escape sequences. If the
//   last cell was red on black with bold, and the next cell is also red on
//   black with bold, we just output the character — no SGR sequences at all.
//
// Together these reduce frame output from thousands of small writes with
// redundant escapes to a single write with minimal escapes.

use std::io::{self, Write};

use crate::ansi;
use crate::cell::{Attr, Cell, UnderlineStyle};
use crate::color::CellColor;

// ─── OutputBuffer ────────────────────────────────────────────────────────────

/// A byte buffer that accumulates ANSI output for a single `write()` syscall.
///
/// Instead of hundreds of small writes per frame (cursor moves, color changes,
/// characters), everything goes into this buffer first. A single flush at
/// frame end writes it all at once, reducing syscall overhead dramatically.
///
/// Default capacity: 16 KB — enough for most frames without reallocation.
pub struct OutputBuffer {
    buf: Vec<u8>,
}

const DEFAULT_CAPACITY: usize = 16_384;

impl OutputBuffer {
    /// Create an empty buffer with default capacity (16 KB).
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(DEFAULT_CAPACITY),
        }
    }

    /// Number of bytes accumulated.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The accumulated bytes (for testing and debugging).
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Write a Unicode codepoint as UTF-8.
    ///
    /// Invalid codepoints (including 0, the continuation marker) produce `?`.
    pub fn write_codepoint(&mut self, cp: u32) {
        if cp == 0 {
            // Continuation cell marker — should never reach output.
            // Defensive fallback: output ? instead of null byte.
            self.buf.push(b'?');
            return;
        }
        match char::from_u32(cp) {
            Some(ch) => {
                let mut enc = [0u8; 4];
                let s = ch.encode_utf8(&mut enc);
                self.buf.extend_from_slice(s.as_bytes());
            }
            None => self.buf.push(b'?'),
        }
    }

    /// Clear the buffer for reuse (keeps allocated capacity).
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Write accumulated output to stdout and clear the buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to stdout fails.
    pub fn flush_stdout(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&self.buf)?;
            stdout.flush()?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Write accumulated output to an arbitrary writer and clear the buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to `w` fails.
    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        if !self.buf.is_empty() {
            w.write_all(&self.buf)?;
            w.flush()?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl Write for OutputBuffer {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Intentionally a no-op. Real flushing via flush_stdout() / flush_to().
        Ok(())
    }
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ─── CellWriter ──────────────────────────────────────────────────────────────

/// Stateful cell renderer that tracks terminal state to skip redundant escapes.
///
/// By remembering the last cursor position, colors, attributes, and underline
/// style, we avoid emitting escape sequences that wouldn't change anything.
///
/// # Optimization decisions
///
/// - **Cursor**: Skipped when the next cell is at `(last_x + 1, last_y)` —
///   the terminal auto-advances after character output.
/// - **Attributes**: On change, reset (SGR 0) + re-emit. This invalidates
///   color and underline tracking, forcing re-emit. When going from no-attrs
///   to attrs, the reset is skipped (nothing to clear).
/// - **Colors**: Skipped if unchanged since last emit.
/// - **Underline**: Tracked separately from attrs for our 6-style system.
/// - **Wide chars**: Continuation cells skip output when preceded by their
///   wide char start (the terminal already drew both columns).
#[allow(clippy::struct_field_names)] // The `last_` prefix IS the semantic grouping.
pub struct CellWriter {
    last_x: i32,
    last_y: i32,
    last_fg: Option<CellColor>,
    last_bg: Option<CellColor>,
    last_attrs: Attr,
    last_underline: UnderlineStyle,
}

impl CellWriter {
    /// Create a writer with no tracked state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_x: -1,
            last_y: -1,
            last_fg: None,
            last_bg: None,
            last_attrs: Attr::empty(),
            last_underline: UnderlineStyle::None,
        }
    }

    /// Reset all tracked state. Call after a terminal reset or screen clear.
    #[allow(clippy::missing_const_for_fn)] // *self = Self::new() isn't const-evaluable.
    pub fn reset_state(&mut self) {
        *self = Self::new();
    }

    /// Render a single cell, emitting only the escape sequences needed.
    pub fn render_cell(&mut self, out: &mut OutputBuffer, x: u16, y: u16, cell: &Cell) {
        let xi = i32::from(x);
        let yi = i32::from(y);

        // ── Cursor positioning ──
        // Skip if the terminal cursor is already here (sequential cell).
        if yi != self.last_y || xi != self.last_x + 1 {
            ansi::cursor_to(out, x, y).ok();
        }

        // ── Continuation cells (wide char second column) ──
        if cell.is_continuation() {
            // If we just rendered the wide char start at (x-1, y), the
            // terminal already drew this position. Skip output.
            if xi > 0 && self.last_x == xi - 1 && self.last_y == yi {
                self.last_x = xi;
                return;
            }
            // Otherwise, output a space with correct background.
            self.apply_style(out, cell);
            out.buf.push(b' ');
            self.last_x = xi;
            self.last_y = yi;
            return;
        }

        // ── Style: attrs, underline, colors ──
        self.apply_style(out, cell);

        // ── Character ──
        out.write_codepoint(cell.ch);

        self.last_x = xi;
        self.last_y = yi;
    }

    /// Apply style changes (attrs, underline, fg, bg) for a cell.
    fn apply_style(&mut self, out: &mut OutputBuffer, cell: &Cell) {
        // Attributes changed: reset if old attrs existed, then emit new ones.
        if cell.attrs != self.last_attrs {
            if !self.last_attrs.is_empty() {
                // SGR 0 clears everything — invalidate all tracking.
                ansi::reset(out).ok();
                self.last_fg = None;
                self.last_bg = None;
                self.last_underline = UnderlineStyle::None;
            }
            self.last_attrs = cell.attrs;
            if !cell.attrs.is_empty() {
                ansi::attrs(out, cell.attrs).ok();
            }
        }

        // Underline style (tracked independently from attrs).
        if cell.underline != self.last_underline {
            ansi::underline(out, cell.underline).ok();
            self.last_underline = cell.underline;
        }

        // Foreground color.
        if self.last_fg != Some(cell.fg) {
            ansi::fg(out, cell.fg).ok();
            self.last_fg = Some(cell.fg);
        }

        // Background color.
        if self.last_bg != Some(cell.bg) {
            ansi::bg(out, cell.bg).ok();
            self.last_bg = Some(cell.bg);
        }
    }
}

impl Default for CellWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── OutputBuffer ────────────────────────────────────────────────────

    #[test]
    fn output_buffer_new_is_empty() {
        let buf = OutputBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn output_buffer_write_trait() {
        let mut buf = OutputBuffer::new();
        write!(buf, "hello {}", 42).unwrap();
        assert_eq!(buf.as_bytes(), b"hello 42");
        assert_eq!(buf.len(), 8);
    }

    #[test]
    fn output_buffer_write_codepoint_ascii() {
        let mut buf = OutputBuffer::new();
        buf.write_codepoint(u32::from('A'));
        assert_eq!(buf.as_bytes(), b"A");
    }

    #[test]
    fn output_buffer_write_codepoint_unicode() {
        let mut buf = OutputBuffer::new();
        buf.write_codepoint(u32::from('中'));
        assert_eq!(buf.as_bytes(), "中".as_bytes());
    }

    #[test]
    fn output_buffer_write_codepoint_emoji() {
        let mut buf = OutputBuffer::new();
        buf.write_codepoint(u32::from('🔥'));
        assert_eq!(buf.as_bytes(), "🔥".as_bytes());
    }

    #[test]
    fn output_buffer_write_codepoint_zero() {
        let mut buf = OutputBuffer::new();
        buf.write_codepoint(0); // continuation marker
        assert_eq!(buf.as_bytes(), b"?");
    }

    #[test]
    fn output_buffer_write_codepoint_invalid() {
        let mut buf = OutputBuffer::new();
        buf.write_codepoint(0xD800); // surrogate, not valid
        assert_eq!(buf.as_bytes(), b"?");
    }

    #[test]
    fn output_buffer_clear_keeps_capacity() {
        let mut buf = OutputBuffer::new();
        write!(buf, "some data").unwrap();
        let cap = buf.buf.capacity();
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.buf.capacity(), cap);
    }

    #[test]
    fn output_buffer_flush_to() {
        let mut buf = OutputBuffer::new();
        write!(buf, "frame data").unwrap();

        let mut dest = Vec::new();
        buf.flush_to(&mut dest).unwrap();

        assert_eq!(dest, b"frame data");
        assert!(buf.is_empty()); // cleared after flush
    }

    #[test]
    fn output_buffer_flush_to_empty_is_noop() {
        let mut buf = OutputBuffer::new();
        let mut dest = Vec::new();
        buf.flush_to(&mut dest).unwrap();
        assert!(dest.is_empty());
    }

    // ── CellWriter — helpers ────────────────────────────────────────────

    /// Render one cell and return the output as a string.
    fn render_one(x: u16, y: u16, cell: &Cell) -> String {
        let mut out = OutputBuffer::new();
        let mut writer = CellWriter::new();
        writer.render_cell(&mut out, x, y, cell);
        String::from_utf8(out.as_bytes().to_vec()).unwrap()
    }

    /// Render a sequence of cells and return the output as a string.
    fn render_seq(cells: &[(u16, u16, Cell)]) -> String {
        let mut out = OutputBuffer::new();
        let mut writer = CellWriter::new();
        for &(x, y, ref cell) in cells {
            writer.render_cell(&mut out, x, y, cell);
        }
        String::from_utf8(out.as_bytes().to_vec()).unwrap()
    }

    // ── CellWriter — cursor ─────────────────────────────────────────────

    #[test]
    fn first_cell_emits_cursor_move() {
        let output = render_one(5, 3, &Cell::new('A'));
        assert!(output.contains("\x1b[4;6H")); // cursor to (5, 3)
        assert!(output.contains('A'));
    }

    #[test]
    fn sequential_cells_skip_cursor_move() {
        let output = render_seq(&[
            (0, 0, Cell::new('A')),
            (1, 0, Cell::new('B')),
            (2, 0, Cell::new('C')),
        ]);
        // Only the first cell gets a cursor move. Cursor-to ends with 'H';
        // our test chars (A, B, C) won't be confused with it.
        let cursor_moves = output.matches('H').count();
        assert_eq!(cursor_moves, 1);
        // Characters should appear as a contiguous run (no escapes between).
        assert!(output.contains("ABC"));
    }

    #[test]
    fn non_sequential_cell_emits_cursor_move() {
        let output = render_seq(&[
            (0, 0, Cell::new('A')),
            (5, 0, Cell::new('B')), // gap — needs cursor move
        ]);
        // Should have two cursor moves.
        let h_count = output.matches('H').count();
        assert_eq!(h_count, 2);
    }

    #[test]
    fn different_row_emits_cursor_move() {
        let output = render_seq(&[
            (0, 0, Cell::new('A')),
            (0, 1, Cell::new('B')), // new row
        ]);
        let h_count = output.matches('H').count();
        assert_eq!(h_count, 2);
    }

    // ── CellWriter — colors ─────────────────────────────────────────────

    #[test]
    fn same_fg_not_re_emitted() {
        let red = CellColor::Rgb(255, 0, 0);
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_fg(red)),
            (1, 0, Cell::new('B').with_fg(red)),
        ]);
        // The fg sequence should appear exactly once.
        let fg_count = output.matches("\x1b[38;2;255;0;0m").count();
        assert_eq!(fg_count, 1);
    }

    #[test]
    fn different_fg_emitted() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_fg(CellColor::Rgb(255, 0, 0))),
            (1, 0, Cell::new('B').with_fg(CellColor::Rgb(0, 255, 0))),
        ]);
        assert!(output.contains("\x1b[38;2;255;0;0m"));
        assert!(output.contains("\x1b[38;2;0;255;0m"));
    }

    #[test]
    fn same_bg_not_re_emitted() {
        let blue = CellColor::Rgb(0, 0, 255);
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_bg(blue)),
            (1, 0, Cell::new('B').with_bg(blue)),
        ]);
        let bg_count = output.matches("\x1b[48;2;0;0;255m").count();
        assert_eq!(bg_count, 1);
    }

    #[test]
    fn different_bg_emitted() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_bg(CellColor::Rgb(0, 0, 255))),
            (1, 0, Cell::new('B').with_bg(CellColor::Rgb(255, 0, 0))),
        ]);
        assert!(output.contains("\x1b[48;2;0;0;255m"));
        assert!(output.contains("\x1b[48;2;255;0;0m"));
    }

    #[test]
    fn default_fg_emitted_on_first_cell() {
        let output = render_one(0, 0, &Cell::new('A'));
        // Default fg should be emitted because last_fg starts as None.
        assert!(output.contains("\x1b[39m"));
    }

    // ── CellWriter — attributes ─────────────────────────────────────────

    #[test]
    fn attrs_emitted_when_set() {
        let output = render_one(0, 0, &Cell::new('A').with_attrs(Attr::BOLD));
        assert!(output.contains("\x1b[1m"));
    }

    #[test]
    fn attr_change_triggers_reset() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_attrs(Attr::BOLD)),
            (1, 0, Cell::new('B').with_attrs(Attr::ITALIC)),
        ]);
        // Switching from BOLD to ITALIC should reset, then emit ITALIC.
        assert!(output.contains("\x1b[0m")); // reset
        assert!(output.contains("\x1b[3m")); // italic
    }

    #[test]
    fn attr_to_none_resets() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_attrs(Attr::BOLD)),
            (1, 0, Cell::new('B')), // no attrs
        ]);
        assert!(output.contains("\x1b[0m")); // reset to clear BOLD
    }

    #[test]
    fn none_to_attr_skips_reset() {
        let output = render_seq(&[
            (0, 0, Cell::new('A')), // no attrs
            (1, 0, Cell::new('B').with_attrs(Attr::BOLD)),
        ]);
        // Going from no-attrs to BOLD shouldn't need a reset.
        assert!(!output.contains("\x1b[0m"));
        assert!(output.contains("\x1b[1m"));
    }

    #[test]
    fn attr_reset_forces_color_re_emit() {
        let red = CellColor::Rgb(255, 0, 0);
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_fg(red).with_attrs(Attr::BOLD)),
            (1, 0, Cell::new('B').with_fg(red).with_attrs(Attr::ITALIC)),
        ]);
        // The attr change triggers reset, which clears fg.
        // So fg must be re-emitted even though it's the same red.
        let fg_count = output.matches("\x1b[38;2;255;0;0m").count();
        assert_eq!(fg_count, 2);
    }

    // ── CellWriter — underline ──────────────────────────────────────────

    #[test]
    fn underline_emitted_when_set() {
        let output = render_one(
            0,
            0,
            &Cell::new('A').with_underline(UnderlineStyle::Curly),
        );
        assert!(output.contains("\x1b[4:3m"));
    }

    #[test]
    fn underline_change_emitted() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_underline(UnderlineStyle::Straight)),
            (1, 0, Cell::new('B').with_underline(UnderlineStyle::Curly)),
        ]);
        assert!(output.contains("\x1b[4:1m")); // straight
        assert!(output.contains("\x1b[4:3m")); // curly
    }

    #[test]
    fn same_underline_not_re_emitted() {
        let output = render_seq(&[
            (0, 0, Cell::new('A').with_underline(UnderlineStyle::Curly)),
            (1, 0, Cell::new('B').with_underline(UnderlineStyle::Curly)),
        ]);
        let count = output.matches("\x1b[4:3m").count();
        assert_eq!(count, 1);
    }

    // ── CellWriter — wide chars / continuation ──────────────────────────

    #[test]
    fn continuation_after_wide_char_skipped() {
        let wide_cell = Cell::styled(
            '中',
            CellColor::Default,
            CellColor::Default,
            Attr::empty(),
            UnderlineStyle::None,
        );
        let cont_cell = Cell::continuation(CellColor::Default, CellColor::Default, Attr::empty());

        let output = render_seq(&[
            (3, 0, wide_cell),
            (4, 0, cont_cell), // should be skipped
        ]);

        // The output should contain '中' but NOT a space for the continuation.
        assert!(output.contains('中'));
        // The continuation cell should produce no visible output.
        // Count characters after the last escape sequence.
        let last_m = output.rfind('m').unwrap();
        let after = &output[last_m + 1..];
        // Should just be the wide char, no trailing space.
        assert_eq!(after, "中");
    }

    #[test]
    fn continuation_without_wide_char_emits_space() {
        // Continuation cell rendered without its wide char parent.
        let cont_cell = Cell::continuation(
            CellColor::Default,
            CellColor::Rgb(0, 0, 255),
            Attr::empty(),
        );

        let output = render_one(4, 0, &cont_cell);

        // Should emit a space (for bg fill) with a cursor move.
        assert!(output.contains("\x1b[1;5H")); // cursor to (4, 0)
        assert!(output.ends_with(' '));
    }
}
