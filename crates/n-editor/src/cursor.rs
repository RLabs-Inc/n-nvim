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
use crate::word;

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

    // -- Word motions -------------------------------------------------------

    /// Move forward to the start of the next word. This is `w` in Vim.
    /// Resets sticky column.
    pub fn word_forward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::word_forward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move backward to the start of the previous word. This is `b` in Vim.
    /// Resets sticky column.
    pub fn word_backward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::word_backward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move forward to the end of the current or next word. This is `e` in Vim.
    /// Resets sticky column.
    pub fn word_end_forward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::word_end_forward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move forward to the start of the next WORD. This is `W` in Vim.
    /// Resets sticky column.
    pub fn big_word_forward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::big_word_forward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move backward to the start of the previous WORD. This is `B` in Vim.
    /// Resets sticky column.
    pub fn big_word_backward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::big_word_backward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    /// Move forward to the end of the current or next WORD. This is `E` in Vim.
    /// Resets sticky column.
    pub fn big_word_end_forward(&mut self, count: usize, buf: &Buffer, past_end: bool) {
        for _ in 0..count {
            self.pos = word::big_word_end_forward(buf, self.pos);
        }
        let max_col = max_col_for_line(buf, self.pos.line, past_end);
        self.pos.col = self.pos.col.min(max_col);
        self.sticky_col = self.pos.col;
    }

    // -- Character find motions ---------------------------------------------

    /// Move forward to the `count`th occurrence of `ch` on the current line.
    /// This is `f{ch}` in Vim. Returns `true` if the cursor moved.
    /// Resets sticky column.
    pub fn char_find_forward(
        &mut self,
        buf: &Buffer,
        ch: char,
        count: usize,
        past_end: bool,
    ) -> bool {
        if let Some(col) = find_on_line_forward(buf, self.pos.line, self.pos.col, ch, count) {
            let max = max_col_for_line(buf, self.pos.line, past_end);
            self.pos.col = col.min(max);
            self.sticky_col = self.pos.col;
            true
        } else {
            false
        }
    }

    /// Move forward to just before the `count`th occurrence of `ch` on the
    /// current line. This is `t{ch}` in Vim. Returns `true` if the cursor
    /// moved to a new position.
    /// Resets sticky column.
    pub fn char_till_forward(
        &mut self,
        buf: &Buffer,
        ch: char,
        count: usize,
        past_end: bool,
    ) -> bool {
        if let Some(col) = find_on_line_forward(buf, self.pos.line, self.pos.col, ch, count) {
            // `t` lands one before the found character.
            let target = col.saturating_sub(1);
            if target > self.pos.col {
                let max = max_col_for_line(buf, self.pos.line, past_end);
                self.pos.col = target.min(max);
                self.sticky_col = self.pos.col;
                true
            } else {
                // Found char is immediately after cursor — nowhere to go.
                false
            }
        } else {
            false
        }
    }

    /// Move backward to the `count`th occurrence of `ch` on the current line.
    /// This is `F{ch}` in Vim. Returns `true` if the cursor moved.
    /// Resets sticky column.
    pub fn char_find_backward(
        &mut self,
        buf: &Buffer,
        ch: char,
        count: usize,
        _past_end: bool,
    ) -> bool {
        if let Some(col) = find_on_line_backward(buf, self.pos.line, self.pos.col, ch, count) {
            self.pos.col = col;
            self.sticky_col = self.pos.col;
            true
        } else {
            false
        }
    }

    /// Move backward to just after the `count`th occurrence of `ch` on the
    /// current line. This is `T{ch}` in Vim. Returns `true` if the cursor
    /// moved to a new position.
    /// Resets sticky column.
    pub fn char_till_backward(
        &mut self,
        buf: &Buffer,
        ch: char,
        count: usize,
        _past_end: bool,
    ) -> bool {
        if let Some(col) = find_on_line_backward(buf, self.pos.line, self.pos.col, ch, count) {
            let target = col + 1;
            if target < self.pos.col {
                self.pos.col = target;
                self.sticky_col = self.pos.col;
                true
            } else {
                // Found char is immediately before cursor — nowhere to go.
                false
            }
        } else {
            false
        }
    }

    // -- Paragraph motions --------------------------------------------------

    /// Move forward to the next paragraph boundary. This is `}` in Vim.
    ///
    /// A paragraph boundary is a blank line (content length == 0). If the
    /// cursor is on a blank line, skip consecutive blanks first, then skip
    /// non-blank lines to reach the next blank line. If on a non-blank line,
    /// find the next blank line directly. Always lands at column 0.
    /// Resets sticky column.
    pub fn paragraph_forward(&mut self, count: usize, buf: &Buffer, _past_end: bool) {
        let line_count = buf.line_count();
        if line_count == 0 {
            return;
        }

        for _ in 0..count {
            let mut i = self.pos.line + 1;

            // If on a blank line, skip consecutive blanks first.
            if buf.line_content_len(self.pos.line).unwrap_or(0) == 0 {
                while i < line_count && buf.line_content_len(i).unwrap_or(0) == 0 {
                    i += 1;
                }
            }

            // Skip non-blank lines to the next blank.
            while i < line_count && buf.line_content_len(i).unwrap_or(0) != 0 {
                i += 1;
            }

            self.pos.line = i.min(line_count.saturating_sub(1));
        }

        self.pos.col = 0;
        self.sticky_col = 0;
    }

    /// Move backward to the previous paragraph boundary. This is `{` in Vim.
    ///
    /// A paragraph boundary is a blank line (content length == 0). If the
    /// cursor is on a blank line, skip consecutive blanks first, then skip
    /// non-blank lines backward to reach the previous blank line. If on a
    /// non-blank line, find the previous blank line directly. Always lands
    /// at column 0. Falls back to line 0 if no blank line exists above.
    /// Resets sticky column.
    pub fn paragraph_backward(&mut self, count: usize, buf: &Buffer, _past_end: bool) {
        if buf.line_count() == 0 {
            return;
        }

        for _ in 0..count {
            if self.pos.line == 0 {
                break;
            }

            let mut i = self.pos.line.saturating_sub(1);

            // If on a blank line, skip consecutive blanks backward first.
            if buf.line_content_len(self.pos.line).unwrap_or(0) == 0 {
                while i > 0 && buf.line_content_len(i).unwrap_or(0) == 0 {
                    i -= 1;
                }
            }

            // Skip non-blank lines backward to the previous blank line.
            // If line 0 is non-blank, we land on line 0 (start of buffer).
            while i > 0 && buf.line_content_len(i).unwrap_or(0) != 0 {
                i -= 1;
            }

            self.pos.line = i;
        }

        self.pos.col = 0;
        self.sticky_col = 0;
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

/// Find the `count`th occurrence of `ch` forward from `from_col` (exclusive)
/// on the given line. Returns the column of the match, or `None`.
fn find_on_line_forward(
    buf: &Buffer,
    line: usize,
    from_col: usize,
    ch: char,
    count: usize,
) -> Option<usize> {
    let rope_line = buf.line(line)?;
    let content_len = buf.line_content_len(line).unwrap_or(0);
    let mut found = 0;
    for i in (from_col + 1)..content_len {
        if rope_line.char(i) == ch {
            found += 1;
            if found == count {
                return Some(i);
            }
        }
    }
    None
}

/// Find the `count`th occurrence of `ch` backward from `from_col` (exclusive)
/// on the given line. Returns the column of the match, or `None`.
fn find_on_line_backward(
    buf: &Buffer,
    line: usize,
    from_col: usize,
    ch: char,
    count: usize,
) -> Option<usize> {
    let rope_line = buf.line(line)?;
    let mut found = 0;
    for i in (0..from_col).rev() {
        if rope_line.char(i) == ch {
            found += 1;
            if found == count {
                return Some(i);
            }
        }
    }
    None
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

    // -- Word motions -------------------------------------------------------

    #[test]
    fn word_forward_basic() {
        let buf = Buffer::from_text("hello world foo");
        let mut c = Cursor::new();

        c.word_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(0, 6));
        assert_eq!(c.sticky_col(), 6);
    }

    #[test]
    fn word_forward_with_count() {
        let buf = Buffer::from_text("one two three four");
        let mut c = Cursor::new();

        c.word_forward(3, &buf, false);
        assert_eq!(c.position(), Position::new(0, 14));
    }

    #[test]
    fn word_backward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::at(Position::new(0, 6));

        c.word_backward(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);
        assert_eq!(c.sticky_col(), 0);
    }

    #[test]
    fn word_backward_with_count() {
        let buf = Buffer::from_text("one two three four");
        let mut c = Cursor::at(Position::new(0, 14));

        c.word_backward(2, &buf, false);
        assert_eq!(c.position(), Position::new(0, 4));
    }

    #[test]
    fn word_end_forward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::new();

        c.word_end_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(0, 4));
        assert_eq!(c.sticky_col(), 4);
    }

    #[test]
    fn word_end_forward_with_count() {
        let buf = Buffer::from_text("one two three");
        let mut c = Cursor::new();

        c.word_end_forward(2, &buf, false);
        assert_eq!(c.position(), Position::new(0, 6));
    }

    #[test]
    fn big_word_forward_basic() {
        let buf = Buffer::from_text("hello.world next");
        let mut c = Cursor::new();

        c.big_word_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(0, 12));
    }

    #[test]
    fn big_word_backward_basic() {
        let buf = Buffer::from_text("hello.world next");
        let mut c = Cursor::at(Position::new(0, 12));

        c.big_word_backward(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);
    }

    #[test]
    fn big_word_end_forward_basic() {
        let buf = Buffer::from_text("hello.world next");
        let mut c = Cursor::new();

        c.big_word_end_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(0, 10));
    }

    #[test]
    fn word_motion_resets_sticky_col() {
        let buf = Buffer::from_text("short\nverylongline\nhi");
        let mut c = Cursor::at(Position::new(1, 11)); // in "verylongline"
        assert_eq!(c.sticky_col(), 11);

        c.word_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(2, 0));
        assert_eq!(c.sticky_col(), 0); // reset by word motion
    }

    #[test]
    fn word_forward_clamps_past_end() {
        // On an empty line in normal mode, cursor must be at col 0.
        let buf = Buffer::from_text("hello\n\nworld");
        let mut c = Cursor::new();

        c.word_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(1, 0));
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

    // -- Character find helpers ---------------------------------------------

    #[test]
    fn find_forward_basic() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(find_on_line_forward(&buf, 0, 0, 'o', 1), Some(4));
    }

    #[test]
    fn find_forward_second_occurrence() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(find_on_line_forward(&buf, 0, 0, 'o', 2), Some(7));
    }

    #[test]
    fn find_forward_not_found() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(find_on_line_forward(&buf, 0, 0, 'z', 1), None);
    }

    #[test]
    fn find_forward_count_exceeds() {
        let buf = Buffer::from_text("hello world");
        // Only 2 'o's, asking for 3rd.
        assert_eq!(find_on_line_forward(&buf, 0, 0, 'o', 3), None);
    }

    #[test]
    fn find_forward_excludes_cursor() {
        let buf = Buffer::from_text("ooo");
        // Cursor on first 'o' (col 0), finds second 'o' at col 1.
        assert_eq!(find_on_line_forward(&buf, 0, 0, 'o', 1), Some(1));
    }

    #[test]
    fn find_forward_at_end_of_line() {
        let buf = Buffer::from_text("abc");
        // Cursor at col 2 ('c'), nothing after it.
        assert_eq!(find_on_line_forward(&buf, 0, 2, 'a', 1), None);
    }

    #[test]
    fn find_backward_basic() {
        let buf = Buffer::from_text("hello world");
        // From col 7 ('o' in "world"), find 'l' backward.
        assert_eq!(find_on_line_backward(&buf, 0, 7, 'l', 1), Some(3));
    }

    #[test]
    fn find_backward_second_occurrence() {
        let buf = Buffer::from_text("hello world");
        // From col 10 ('d'), find 'l' backward: first is col 9, second is col 3.
        assert_eq!(find_on_line_backward(&buf, 0, 10, 'l', 2), Some(3));
    }

    #[test]
    fn find_backward_not_found() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(find_on_line_backward(&buf, 0, 5, 'z', 1), None);
    }

    #[test]
    fn find_backward_excludes_cursor() {
        let buf = Buffer::from_text("ooo");
        // Cursor on last 'o' (col 2), finds 'o' at col 1.
        assert_eq!(find_on_line_backward(&buf, 0, 2, 'o', 1), Some(1));
    }

    #[test]
    fn find_backward_at_start() {
        let buf = Buffer::from_text("abc");
        // Cursor at col 0, nothing before it.
        assert_eq!(find_on_line_backward(&buf, 0, 0, 'a', 1), None);
    }

    // -- char_find_forward --------------------------------------------------

    #[test]
    fn char_find_forward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::new();

        assert!(c.char_find_forward(&buf, 'w', 1, false));
        assert_eq!(c.col(), 6);
        assert_eq!(c.sticky_col(), 6);
    }

    #[test]
    fn char_find_forward_not_found() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::new();

        assert!(!c.char_find_forward(&buf, 'z', 1, false));
        assert_eq!(c.col(), 0); // didn't move
    }

    #[test]
    fn char_find_forward_with_count() {
        let buf = Buffer::from_text("abracadabra");
        let mut c = Cursor::new();

        assert!(c.char_find_forward(&buf, 'a', 3, false));
        assert_eq!(c.col(), 7); // 3rd 'a' after col 0: cols 3, 5, 7
    }

    // -- char_find_backward -------------------------------------------------

    #[test]
    fn char_find_backward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::at(Position::new(0, 10));

        assert!(c.char_find_backward(&buf, 'o', 1, false));
        assert_eq!(c.col(), 7);
    }

    #[test]
    fn char_find_backward_not_found() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::new();

        assert!(!c.char_find_backward(&buf, 'z', 1, false));
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn char_find_backward_with_count() {
        let buf = Buffer::from_text("abracadabra");
        let mut c = Cursor::at(Position::new(0, 10));

        // From 'a' at col 10, find 2nd 'a' backward: col 7, then col 5.
        assert!(c.char_find_backward(&buf, 'a', 2, false));
        assert_eq!(c.col(), 5);
    }

    // -- char_till_forward --------------------------------------------------

    #[test]
    fn char_till_forward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::new();

        // `to` → lands at col 3 (one before 'o' at col 4).
        assert!(c.char_till_forward(&buf, 'o', 1, false));
        assert_eq!(c.col(), 3);
    }

    #[test]
    fn char_till_forward_adjacent_no_move() {
        let buf = Buffer::from_text("ab");
        let mut c = Cursor::new(); // col 0

        // `tb` → 'b' is at col 1. Target = col 0 = cursor. No movement.
        assert!(!c.char_till_forward(&buf, 'b', 1, false));
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn char_till_forward_not_found() {
        let buf = Buffer::from_text("hello");
        let mut c = Cursor::new();

        assert!(!c.char_till_forward(&buf, 'z', 1, false));
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn char_till_forward_with_count() {
        let buf = Buffer::from_text("aXbXcXd");
        let mut c = Cursor::new(); // col 0

        // `2tX` → 2nd 'X' is at col 3. Target = col 2 (one before).
        assert!(c.char_till_forward(&buf, 'X', 2, false));
        assert_eq!(c.col(), 2);
    }

    // -- char_till_backward -------------------------------------------------

    #[test]
    fn char_till_backward_basic() {
        let buf = Buffer::from_text("hello world");
        let mut c = Cursor::at(Position::new(0, 10));

        // `To` → 'o' at col 7. Target = col 8 (one after).
        assert!(c.char_till_backward(&buf, 'o', 1, false));
        assert_eq!(c.col(), 8);
    }

    #[test]
    fn char_till_backward_adjacent_no_move() {
        let buf = Buffer::from_text("ba");
        let mut c = Cursor::at(Position::new(0, 1)); // col 1

        // `Tb` → 'b' at col 0. Target = col 1 = cursor. No movement.
        assert!(!c.char_till_backward(&buf, 'b', 1, false));
        assert_eq!(c.col(), 1);
    }

    #[test]
    fn char_till_backward_not_found() {
        let buf = Buffer::from_text("hello");
        let mut c = Cursor::at(Position::new(0, 4));

        assert!(!c.char_till_backward(&buf, 'z', 1, false));
        assert_eq!(c.col(), 4);
    }

    #[test]
    fn char_till_backward_with_count() {
        let buf = Buffer::from_text("aXbXcXd");
        let mut c = Cursor::at(Position::new(0, 6)); // col 6 ('d')

        // `2TX` → 2nd 'X' back from col 6 is col 3. Target = col 4.
        assert!(c.char_till_backward(&buf, 'X', 2, false));
        assert_eq!(c.col(), 4);
    }

    // -- char find on multiline buffer (stays on current line) ---------------

    #[test]
    fn char_find_forward_stays_on_line() {
        let buf = Buffer::from_text("abc\ndef");
        let mut c = Cursor::new();

        // 'd' is on line 1, not reachable from line 0.
        assert!(!c.char_find_forward(&buf, 'd', 1, false));
        assert_eq!(c.col(), 0);
    }

    #[test]
    fn char_find_backward_stays_on_line() {
        let buf = Buffer::from_text("abc\ndef");
        let mut c = Cursor::at(Position::new(1, 2)); // 'f' on line 1

        // 'a' is on line 0, not reachable from line 1.
        assert!(!c.char_find_backward(&buf, 'a', 1, false));
        assert_eq!(c.col(), 2);
    }

    // -- Paragraph motions --------------------------------------------------

    #[test]
    fn paragraph_forward_to_blank_line() {
        let buf = Buffer::from_text("aaa\nbbb\n\nccc");
        let mut c = Cursor::new();

        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(2, 0));
    }

    #[test]
    fn paragraph_forward_from_blank_line() {
        let buf = Buffer::from_text("aaa\n\nbbb\n\nccc");
        let mut c = Cursor::at(Position::new(1, 0)); // blank line

        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(3, 0));
    }

    #[test]
    fn paragraph_forward_to_end_of_buffer() {
        let buf = Buffer::from_text("aaa\nbbb\nccc");
        let mut c = Cursor::new();

        // No blank lines → go to last line.
        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.position(), Position::new(2, 0));
    }

    #[test]
    fn paragraph_forward_with_count() {
        let buf = Buffer::from_text("a\n\nb\n\nc");
        let mut c = Cursor::new();

        c.paragraph_forward(2, &buf, false);
        assert_eq!(c.position(), Position::new(3, 0));
    }

    #[test]
    fn paragraph_forward_already_at_end() {
        let buf = Buffer::from_text("hello");
        let mut c = Cursor::at(Position::new(0, 3));

        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.line(), 0); // stays on last line
        assert_eq!(c.col(), 0); // resets to column 0
    }

    #[test]
    fn paragraph_forward_consecutive_blanks() {
        let buf = Buffer::from_text("aaa\n\n\nbbb");
        let mut c = Cursor::new();

        // From "aaa", } should go to the first blank line.
        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.line(), 1);
    }

    #[test]
    fn paragraph_backward_to_blank_line() {
        let buf = Buffer::from_text("aaa\n\nbbb\nccc");
        let mut c = Cursor::at(Position::new(3, 0)); // on "ccc"

        c.paragraph_backward(1, &buf, false);
        assert_eq!(c.position(), Position::new(1, 0));
    }

    #[test]
    fn paragraph_backward_from_blank_line() {
        let buf = Buffer::from_text("aaa\n\nbbb\n\nccc");
        let mut c = Cursor::at(Position::new(3, 0)); // blank line

        c.paragraph_backward(1, &buf, false);
        assert_eq!(c.position(), Position::new(1, 0));
    }

    #[test]
    fn paragraph_backward_to_start_of_buffer() {
        let buf = Buffer::from_text("aaa\nbbb\nccc");
        let mut c = Cursor::at(Position::new(2, 0));

        // No blank lines above → go to line 0.
        c.paragraph_backward(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);
    }

    #[test]
    fn paragraph_backward_with_count() {
        let buf = Buffer::from_text("a\n\nb\n\nc");
        let mut c = Cursor::at(Position::new(4, 0)); // on "c"

        c.paragraph_backward(2, &buf, false);
        assert_eq!(c.position(), Position::new(1, 0));
    }

    #[test]
    fn paragraph_backward_already_at_start() {
        let buf = Buffer::from_text("hello");
        let mut c = Cursor::at(Position::new(0, 3));

        c.paragraph_backward(1, &buf, false);
        assert_eq!(c.position(), Position::ZERO);
    }

    #[test]
    fn paragraph_backward_consecutive_blanks() {
        let buf = Buffer::from_text("aaa\n\n\nbbb");
        let mut c = Cursor::at(Position::new(3, 0)); // on "bbb"

        // From "bbb", { finds the nearest blank line above (line 2).
        c.paragraph_backward(1, &buf, false);
        assert_eq!(c.line(), 2);
    }

    #[test]
    fn paragraph_resets_sticky_col() {
        let buf = Buffer::from_text("hello\n\nworld");
        let mut c = Cursor::at(Position::new(0, 4)); // col 4
        assert_eq!(c.sticky_col(), 4);

        c.paragraph_forward(1, &buf, false);
        assert_eq!(c.col(), 0);
        assert_eq!(c.sticky_col(), 0);
    }
}
