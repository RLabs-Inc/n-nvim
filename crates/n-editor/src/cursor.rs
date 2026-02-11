//! Cursor — position tracking with movement and selection.
//!
//! The `Cursor` tracks a position in a buffer, a sticky column for vertical
//! movement, and an optional selection anchor. It provides movement primitives
//! that respect buffer boundaries and cursor limits.
//!
//! # Mode-agnostic design
//!
//! Movement methods take a `past_end: bool` parameter rather than a Mode enum.
//! This keeps the cursor decoupled from any specific input profile:
//!
//! - **Vim normal mode**: `past_end = false` (cursor sits ON a character)
//! - **Vim insert mode**: `past_end = true` (cursor can sit after last char)
//! - **Non-modal mode** (e.g. VSCode-style): always `past_end = true`
//!
//! The caller decides the limit. The cursor just moves within it.
//!
//! # Sticky column
//!
//! When moving vertically, the cursor remembers the column it was at. If it
//! moves through a short line and then reaches a long line again, it snaps
//! back to the remembered column. Horizontal movement resets the sticky column.
//!
//! # Selection
//!
//! The cursor has an optional `anchor` position. When set, the text between
//! `anchor` and the cursor's current position forms a selection. This works
//! for both Vim visual mode (`v` sets anchor) and VSCode-style shift-selection
//! (Shift+Arrow sets anchor on first press).

use crate::buffer::Buffer;
use crate::position::{Position, Range};

/// A cursor in a text buffer.
///
/// Lightweight value type — just a position, a sticky column, and an optional
/// selection anchor. Does not own or reference the buffer; the buffer is
/// passed to movement methods as a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Current position in the buffer.
    pos: Position,

    /// Remembered column for vertical movement. When moving up/down through
    /// lines of varying length, the cursor tries to return to this column.
    /// Horizontal movement resets it to the current column.
    sticky_col: usize,

    /// Selection anchor. When `Some`, the region between `anchor` and `pos`
    /// is selected. The anchor is the "other end" — it stays put while the
    /// cursor moves.
    anchor: Option<Position>,
}

impl Cursor {
    /// Create a cursor at the origin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pos: Position::ZERO,
            sticky_col: 0,
            anchor: None,
        }
    }

    /// Create a cursor at a specific position.
    #[must_use]
    pub const fn at(pos: Position) -> Self {
        Self {
            pos,
            sticky_col: pos.col,
            anchor: None,
        }
    }

    // -- Accessors ----------------------------------------------------------

    /// Current position.
    #[inline]
    #[must_use]
    pub const fn position(&self) -> Position {
        self.pos
    }

    /// Current line (0-indexed).
    #[inline]
    #[must_use]
    pub const fn line(&self) -> usize {
        self.pos.line
    }

    /// Current column (0-indexed, char offset).
    #[inline]
    #[must_use]
    pub const fn col(&self) -> usize {
        self.pos.col
    }

    /// The sticky column (desired column for vertical movement).
    #[inline]
    #[must_use]
    pub const fn sticky_col(&self) -> usize {
        self.sticky_col
    }

    /// The selection anchor, if a selection is active.
    #[inline]
    #[must_use]
    pub const fn anchor(&self) -> Option<Position> {
        self.anchor
    }

    /// True if a selection is active (anchor is set).
    #[inline]
    #[must_use]
    pub const fn has_selection(&self) -> bool {
        self.anchor.is_some()
    }

    /// The selected range, if a selection is active. Returns `None` if no
    /// anchor is set. The range is always ordered (start <= end) regardless
    /// of whether the cursor is before or after the anchor.
    #[must_use]
    pub fn selection(&self) -> Option<Range> {
        self.anchor.map(|anchor| Range::ordered(anchor, self.pos))
    }

    // -- Selection control --------------------------------------------------

    /// Set the selection anchor at the current position. Future movement will
    /// extend the selection from this point.
    pub const fn set_anchor(&mut self) {
        self.anchor = Some(self.pos);
    }

    /// Set the selection anchor at a specific position.
    pub const fn set_anchor_at(&mut self, pos: Position) {
        self.anchor = Some(pos);
    }

    /// Clear the selection (remove the anchor).
    pub const fn clear_anchor(&mut self) {
        self.anchor = None;
    }

    // -- Direct positioning -------------------------------------------------

    /// Move the cursor to an exact position, clamped to buffer bounds.
    ///
    /// Resets the sticky column to the new position's column.
    /// Does **not** affect the selection anchor.
    pub fn set_position(&mut self, pos: Position, buf: &Buffer, past_end: bool) {
        self.pos = clamp(pos, buf, past_end);
        self.sticky_col = self.pos.col;
    }

    /// Move to a specific line, keeping the current column (or clamping).
    /// Resets sticky column.
    pub fn goto_line(&mut self, line: usize, buf: &Buffer, past_end: bool) {
        let target = Position::new(line, self.pos.col);
        self.pos = clamp(target, buf, past_end);
        self.sticky_col = self.pos.col;
    }

    // -- Horizontal movement ------------------------------------------------

    /// Move left by `count` characters. Stops at column 0 (no line wrapping).
    /// Resets sticky column.
    pub fn move_left(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        let col = self.pos.col.min(max_col);
        self.pos.col = col.saturating_sub(count);
        self.sticky_col = self.pos.col;
    }

    /// Move right by `count` characters. Stops at the column limit for the
    /// current line (no line wrapping). Resets sticky column.
    pub fn move_right(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = (self.pos.col + count).min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move to the first column of the current line. Resets sticky column.
    /// This is `0` in Vim.
    pub const fn move_to_line_start(&mut self) {
        self.pos.col = 0;
        self.sticky_col = 0;
    }

    /// Move to the first non-whitespace character of the current line.
    /// This is `^` in Vim. Resets sticky column.
    pub fn move_to_first_non_blank(&mut self, buf: &Buffer, past_end: bool) {
        if let Some(line) = buf.line(self.pos.line) {
            let col = line
                .chars()
                .take_while(|ch| ch.is_whitespace() && *ch != '\n' && *ch != '\r')
                .count();
            let max_col = max_col_for_line(buf, self.pos.line, past_end);
            self.pos.col = col.min(max_col);
        } else {
            self.pos.col = 0;
        }
        self.sticky_col = self.pos.col;
    }

    /// Move to the last character (or past-last in insert mode) of the
    /// current line. This is `$` in Vim. Resets sticky column.
    pub fn move_to_line_end(&mut self, buf: &Buffer, past_end: bool) {
        self.pos.col = max_col_for_line(buf, self.pos.line, past_end);
        self.sticky_col = self.pos.col;
    }

    // -- Vertical movement --------------------------------------------------

    /// Move up by `count` lines. Uses the sticky column to maintain horizontal
    /// position across lines of varying length.
    pub fn move_up(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        self.pos.line = self.pos.line.saturating_sub(count);
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.sticky_col.min(max_col);
    }

    /// Move down by `count` lines. Uses the sticky column to maintain
    /// horizontal position across lines of varying length.
    pub fn move_down(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        let last_line = buf.line_count().saturating_sub(1);
        self.pos.line = (self.pos.line + count).min(last_line);
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.sticky_col.min(max_col);
    }

    /// Move to the first line of the buffer. This is `gg` in Vim.
    /// Preserves the sticky column.
    pub fn move_to_first_line(&mut self, buf: &Buffer, past_end: bool) {
        self.pos.line = 0;
        let max_col = max_col_for_line(buf, 0, past_end);
        self.pos.col = self.sticky_col.min(max_col);
    }

    /// Move to the last line of the buffer. This is `G` in Vim.
    /// Preserves the sticky column.
    pub fn move_to_last_line(&mut self, buf: &Buffer, past_end: bool) {
        let last_line = buf.line_count().saturating_sub(1);
        self.pos.line = last_line;
        let max_col = max_col_for_line(buf, last_line, past_end);
        self.pos.col = self.sticky_col.min(max_col);
    }

    // -- Clamping -----------------------------------------------------------

    /// Ensure the cursor is within buffer bounds. Call this after the buffer
    /// has been modified (lines deleted, content changed) to prevent the
    /// cursor from pointing at invalid positions.
    pub fn clamp(&mut self, buf: &Buffer, past_end: bool) {
        self.pos = clamp(self.pos, buf, past_end);
        // Also clamp anchor if present.
        if let Some(anchor) = &mut self.anchor {
            *anchor = clamp(*anchor, buf, past_end);
        }
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Maximum valid column for a given line and cursor mode.
///
/// - `past_end = false`: cursor must sit ON a character → `content_len - 1`
///   (or 0 for empty lines).
/// - `past_end = true`: cursor can sit after last char → `content_len`.
fn max_col_for_line(buf: &Buffer, line: usize, past_end: bool) -> usize {
    let content_len = buf.line_content_len(line).unwrap_or(0);
    if past_end {
        content_len
    } else {
        // Normal mode: cursor on a character. Empty line → 0.
        content_len.saturating_sub(1)
    }
}

/// Clamp a position to valid buffer bounds.
fn clamp(pos: Position, buf: &Buffer, past_end: bool) -> Position {
    if buf.is_empty() {
        return Position::ZERO;
    }
    let line = pos.line.min(buf.line_count().saturating_sub(1));
    let max_col = max_col_for_line(buf, line, past_end);
    let col = pos.col.min(max_col);
    Position::new(line, col)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a multi-line buffer for testing.
    fn sample_buffer() -> Buffer {
        // 5 lines of varying length:
        // "hello\n"      (5 chars content)
        // "world\n"      (5 chars content)
        // "hi\n"          (2 chars content)
        // "\n"            (0 chars content)
        // "goodbye"      (7 chars content, no trailing newline)
        Buffer::from_text("hello\nworld\nhi\n\ngoodbye")
    }

    // -- Construction -------------------------------------------------------

    #[test]
    fn new_at_origin() {
        let c = Cursor::new();
        assert_eq!(c.position(), Position::ZERO);
        assert_eq!(c.sticky_col(), 0);
        assert!(c.anchor().is_none());
        assert!(!c.has_selection());
    }

    #[test]
    fn at_specific_position() {
        let c = Cursor::at(Position::new(3, 7));
        assert_eq!(c.position(), Position::new(3, 7));
        assert_eq!(c.sticky_col(), 7);
        assert!(c.anchor().is_none());
    }

    #[test]
    fn default_is_new() {
        assert_eq!(Cursor::default(), Cursor::new());
    }

    // -- Selection ----------------------------------------------------------

    #[test]
    fn set_and_clear_anchor() {
        let mut c = Cursor::at(Position::new(1, 3));
        assert!(!c.has_selection());

        c.set_anchor();
        assert!(c.has_selection());
        assert_eq!(c.anchor(), Some(Position::new(1, 3)));

        c.clear_anchor();
        assert!(!c.has_selection());
        assert_eq!(c.anchor(), None);
    }

    #[test]
    fn set_anchor_at_specific() {
        let mut c = Cursor::new();
        c.set_anchor_at(Position::new(5, 10));
        assert_eq!(c.anchor(), Some(Position::new(5, 10)));
    }

    #[test]
    fn selection_range_ordered() {
        let mut c = Cursor::at(Position::new(2, 5));
        c.set_anchor_at(Position::new(0, 3));

        let sel = c.selection().unwrap();
        assert_eq!(sel.start, Position::new(0, 3));
        assert_eq!(sel.end, Position::new(2, 5));
    }

    #[test]
    fn selection_range_anchor_after_cursor() {
        let mut c = Cursor::at(Position::new(0, 2));
        c.set_anchor_at(Position::new(3, 0));

        let sel = c.selection().unwrap();
        // Range::ordered ensures start < end.
        assert_eq!(sel.start, Position::new(0, 2));
        assert_eq!(sel.end, Position::new(3, 0));
    }

    #[test]
    fn selection_none_without_anchor() {
        let c = Cursor::new();
        assert!(c.selection().is_none());
    }

    // -- set_position -------------------------------------------------------

    #[test]
    fn set_position_clamps() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.set_position(Position::new(100, 100), &buf, false);
        // Last line is 4 ("goodbye", 7 chars), max col = 6 in normal mode.
        assert_eq!(c.position(), Position::new(4, 6));
    }

    #[test]
    fn set_position_past_end() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.set_position(Position::new(0, 100), &buf, true);
        // Insert mode: max col = 5 (content_len of "hello").
        assert_eq!(c.position(), Position::new(0, 5));
    }

    #[test]
    fn set_position_resets_sticky() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 4));
        assert_eq!(c.sticky_col(), 4);

        c.set_position(Position::new(1, 2), &buf, false);
        assert_eq!(c.sticky_col(), 2);
    }

    // -- goto_line ----------------------------------------------------------

    #[test]
    fn goto_line_clamps_col() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 4));

        c.goto_line(2, &buf, false); // Line 2 = "hi" (2 chars, max col = 1)
        assert_eq!(c.position(), Position::new(2, 1));
    }

    #[test]
    fn goto_line_out_of_bounds() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.goto_line(100, &buf, false);
        assert_eq!(c.line(), 4); // clamped to last line
    }

    // -- Horizontal movement ------------------------------------------------

    #[test]
    fn move_left_basic() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 3));

        c.move_left(1, &buf, false);
        assert_eq!(c.col(), 2);
        assert_eq!(c.sticky_col(), 2);
    }

    #[test]
    fn move_left_stops_at_zero() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 1));

        c.move_left(5, &buf, false);
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn move_left_from_zero() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_left(1, &buf, false);
        assert_eq!(c.col(), 0); // stays at 0, no wrapping
    }

    #[test]
    fn move_right_basic() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_right(3, &buf, false);
        assert_eq!(c.col(), 3);
    }

    #[test]
    fn move_right_stops_at_limit_normal() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_right(100, &buf, false);
        // "hello" = 5 chars, max col in normal = 4
        assert_eq!(c.col(), 4);
    }

    #[test]
    fn move_right_stops_at_limit_insert() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_right(100, &buf, true);
        // "hello" = 5 chars, max col in insert = 5
        assert_eq!(c.col(), 5);
    }

    #[test]
    fn move_right_resets_sticky() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_right(3, &buf, false);
        assert_eq!(c.sticky_col(), 3);
    }

    #[test]
    fn move_left_count() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 4));

        c.move_left(3, &buf, false);
        assert_eq!(c.col(), 1);
    }

    #[test]
    fn move_right_count() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 1));

        c.move_right(2, &buf, false);
        assert_eq!(c.col(), 3);
    }

    // -- Line start/end -----------------------------------------------------

    #[test]
    fn move_to_line_start() {
        let mut c = Cursor::at(Position::new(2, 5));
        c.move_to_line_start();
        assert_eq!(c.col(), 0);
        assert_eq!(c.sticky_col(), 0);
    }

    #[test]
    fn move_to_line_end_normal() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 1));

        c.move_to_line_end(&buf, false);
        assert_eq!(c.col(), 4); // "hello" last char is col 4
    }

    #[test]
    fn move_to_line_end_insert() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 1));

        c.move_to_line_end(&buf, true);
        assert_eq!(c.col(), 5); // past "hello"
    }

    #[test]
    fn move_to_line_end_empty_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(3, 0)); // empty line

        c.move_to_line_end(&buf, false);
        assert_eq!(c.col(), 0); // empty line, nowhere to go
    }

    // -- First non-blank ----------------------------------------------------

    #[test]
    fn move_to_first_non_blank_no_indent() {
        let buf = Buffer::from_text("hello");
        let mut c = Cursor::at(Position::new(0, 3));

        c.move_to_first_non_blank(&buf, false);
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn move_to_first_non_blank_with_indent() {
        let buf = Buffer::from_text("    hello");
        let mut c = Cursor::new();

        c.move_to_first_non_blank(&buf, false);
        assert_eq!(c.col(), 4); // past the 4 spaces
    }

    #[test]
    fn move_to_first_non_blank_tabs() {
        let buf = Buffer::from_text("\t\thello");
        let mut c = Cursor::new();

        c.move_to_first_non_blank(&buf, false);
        assert_eq!(c.col(), 2); // past 2 tabs
    }

    #[test]
    fn move_to_first_non_blank_all_whitespace() {
        let buf = Buffer::from_text("   \n");
        let mut c = Cursor::new();

        c.move_to_first_non_blank(&buf, false);
        // All whitespace (excluding \n) = 3 spaces, max col normal = 2
        assert_eq!(c.col(), 2);
    }

    #[test]
    fn move_to_first_non_blank_empty_line() {
        let buf = Buffer::from_text("\nhello");
        let mut c = Cursor::new();

        c.move_to_first_non_blank(&buf, false);
        assert_eq!(c.col(), 0);
    }

    // -- Vertical movement --------------------------------------------------

    #[test]
    fn move_down_basic() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 3));

        c.move_down(1, &buf, false);
        assert_eq!(c.position(), Position::new(1, 3));
    }

    #[test]
    fn move_up_basic() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(2, 1));

        c.move_up(1, &buf, false);
        assert_eq!(c.position(), Position::new(1, 1));
    }

    #[test]
    fn move_down_stops_at_last_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(3, 0));

        c.move_down(100, &buf, false);
        assert_eq!(c.line(), 4);
    }

    #[test]
    fn move_up_stops_at_first_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(1, 0));

        c.move_up(100, &buf, false);
        assert_eq!(c.line(), 0);
    }

    #[test]
    fn move_down_count() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_down(3, &buf, false);
        assert_eq!(c.line(), 3);
    }

    #[test]
    fn move_up_count() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(4, 0));

        c.move_up(2, &buf, false);
        assert_eq!(c.line(), 2);
    }

    // -- Sticky column behavior ---------------------------------------------

    #[test]
    fn sticky_col_preserved_through_short_line() {
        let buf = sample_buffer();
        // Start at line 0, col 4 ("hello" — col 4 = 'o').
        let mut c = Cursor::at(Position::new(0, 4));

        // Move to line 2 ("hi" — max col = 1). Cursor snaps to 1.
        c.move_down(2, &buf, false);
        assert_eq!(c.position(), Position::new(2, 1));
        assert_eq!(c.sticky_col(), 4); // remembers 4

        // Move to line 4 ("goodbye" — 7 chars). Cursor snaps back to 4.
        c.move_down(2, &buf, false);
        assert_eq!(c.position(), Position::new(4, 4));
        assert_eq!(c.sticky_col(), 4); // still 4
    }

    #[test]
    fn sticky_col_through_empty_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 3));

        // Move through empty line 3.
        c.move_down(3, &buf, false);
        assert_eq!(c.position(), Position::new(3, 0)); // empty line
        assert_eq!(c.sticky_col(), 3);

        // Continue to line 4 ("goodbye").
        c.move_down(1, &buf, false);
        assert_eq!(c.position(), Position::new(4, 3));
    }

    #[test]
    fn horizontal_movement_resets_sticky() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 4));
        assert_eq!(c.sticky_col(), 4);

        c.move_left(2, &buf, false);
        assert_eq!(c.sticky_col(), 2); // reset to current col

        c.move_down(1, &buf, false);
        // Now uses sticky_col = 2, not the old 4.
        assert_eq!(c.col(), 2);
    }

    #[test]
    fn line_end_resets_sticky() {
        let buf = sample_buffer();
        let mut c = Cursor::new();

        c.move_to_line_end(&buf, false);
        assert_eq!(c.sticky_col(), 4); // end of "hello"

        c.move_down(1, &buf, false);
        assert_eq!(c.col(), 4); // end of "world"
    }

    #[test]
    fn line_start_resets_sticky() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 3));

        c.move_to_line_start();
        assert_eq!(c.sticky_col(), 0);

        c.move_down(1, &buf, false);
        assert_eq!(c.col(), 0);
    }

    // -- First/last line ----------------------------------------------------

    #[test]
    fn move_to_first_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(4, 3));

        c.move_to_first_line(&buf, false);
        assert_eq!(c.line(), 0);
        assert_eq!(c.col(), 3); // sticky col preserved
    }

    #[test]
    fn move_to_last_line() {
        let buf = sample_buffer();
        let mut c = Cursor::at(Position::new(0, 3));

        c.move_to_last_line(&buf, false);
        assert_eq!(c.line(), 4);
        assert_eq!(c.col(), 3); // sticky col preserved
    }

    #[test]
    fn move_to_first_line_sticky_clamps() {
        let buf = Buffer::from_text("hi\nhello world");
        let mut c = Cursor::at(Position::new(1, 10));

        c.move_to_first_line(&buf, false);
        assert_eq!(c.line(), 0);
        assert_eq!(c.col(), 1); // "hi" max normal = 1
        assert_eq!(c.sticky_col(), 10); // still remembered
    }

    #[test]
    fn move_to_last_line_sticky_clamps() {
        let buf = Buffer::from_text("hello world\nhi");
        let mut c = Cursor::at(Position::new(0, 10));

        c.move_to_last_line(&buf, false);
        assert_eq!(c.line(), 1);
        assert_eq!(c.col(), 1); // "hi" max normal = 1
        assert_eq!(c.sticky_col(), 10); // remembered
    }

    // -- Clamping -----------------------------------------------------------

    #[test]
    fn clamp_after_content_deletion() {
        let mut buf = Buffer::from_text("hello\nworld\nfoo");
        let mut c = Cursor::at(Position::new(2, 2)); // on "foo"

        // Delete the last two lines.
        buf.delete(crate::position::Range::new(
            Position::new(0, 5), // the \n at end of "hello"
            Position::new(2, 3), // end of "foo"
        ));
        // Now buffer is just "hello".

        c.clamp(&buf, false);
        assert_eq!(c.line(), 0);
        assert_eq!(c.col(), 2); // within "hello" bounds
    }

    #[test]
    fn clamp_empty_buffer() {
        let buf = Buffer::new();
        let mut c = Cursor::at(Position::new(10, 10));

        c.clamp(&buf, false);
        assert_eq!(c.position(), Position::ZERO);
    }

    #[test]
    fn clamp_also_clamps_anchor() {
        let mut buf = Buffer::from_text("hello\nworld\nfoo");
        let mut c = Cursor::at(Position::new(2, 2));
        c.set_anchor_at(Position::new(1, 4));

        // Delete everything after "hello".
        buf.delete(crate::position::Range::new(
            Position::new(0, 5),
            Position::new(2, 3),
        ));

        c.clamp(&buf, false);
        assert_eq!(c.position(), Position::new(0, 2));
        assert_eq!(c.anchor(), Some(Position::new(0, 4)));
    }

    // -- Empty buffer behavior ----------------------------------------------

    #[test]
    fn movement_on_empty_buffer() {
        let buf = Buffer::new();
        let mut c = Cursor::new();

        c.move_right(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);

        c.move_down(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);

        c.move_to_line_end(&buf, false);
        assert_eq!(c.position(), Position::ZERO);
    }

    #[test]
    fn movement_on_empty_buffer_insert_mode() {
        let buf = Buffer::new();
        let mut c = Cursor::new();

        c.move_right(1, &buf, true);
        assert_eq!(c.position(), Position::ZERO); // empty = 0 content, past_end still 0

        c.move_to_line_end(&buf, true);
        assert_eq!(c.position(), Position::ZERO);
    }

    // -- Single-char buffer -------------------------------------------------

    #[test]
    fn single_char_normal_mode() {
        let buf = Buffer::from_text("x");
        let mut c = Cursor::new();

        c.move_right(1, &buf, false);
        assert_eq!(c.col(), 0); // only one char, max col = 0

        c.move_to_line_end(&buf, false);
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn single_char_insert_mode() {
        let buf = Buffer::from_text("x");
        let mut c = Cursor::new();

        c.move_right(1, &buf, true);
        assert_eq!(c.col(), 1); // can go past

        c.move_to_line_end(&buf, true);
        assert_eq!(c.col(), 1);
    }

    // -- max_col_for_line helper --------------------------------------------

    #[test]
    fn max_col_normal_mode() {
        let buf = sample_buffer();
        // "hello" = 5 chars, normal max = 4
        assert_eq!(max_col_for_line(&buf, 0, false), 4);
        // "hi" = 2 chars, normal max = 1
        assert_eq!(max_col_for_line(&buf, 2, false), 1);
        // empty line, normal max = 0
        assert_eq!(max_col_for_line(&buf, 3, false), 0);
        // "goodbye" = 7 chars, normal max = 6
        assert_eq!(max_col_for_line(&buf, 4, false), 6);
    }

    #[test]
    fn max_col_insert_mode() {
        let buf = sample_buffer();
        // "hello" = 5 chars, insert max = 5
        assert_eq!(max_col_for_line(&buf, 0, true), 5);
        // empty line, insert max = 0
        assert_eq!(max_col_for_line(&buf, 3, true), 0);
    }

    // -- Integration: cursor + buffer edits ---------------------------------

    #[test]
    fn cursor_follows_insertion() {
        let mut buf = Buffer::from_text("hllo");
        let mut c = Cursor::at(Position::new(0, 1));

        // Insert 'e' at cursor position.
        buf.insert(c.position(), "e");
        // After insertion, cursor should move right (caller responsibility).
        c.move_right(1, &buf, true);
        assert_eq!(c.position(), Position::new(0, 2));
        assert_eq!(buf.contents(), "hello");
    }

    #[test]
    fn cursor_clamps_after_deletion() {
        let mut buf = Buffer::from_text("hello world");
        let mut c = Cursor::at(Position::new(0, 10)); // on 'd'

        // Delete "world".
        buf.delete(crate::position::Range::new(
            Position::new(0, 5),
            Position::new(0, 11),
        ));

        c.clamp(&buf, false);
        assert_eq!(c.col(), 4); // "hello" max normal = 4
    }
}
