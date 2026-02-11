//! Undo/redo history — transaction-based edit tracking.
//!
//! Records every buffer mutation as a reversible [`Edit`] grouped into
//! [`Transaction`]s. A transaction is the atomic unit of undo/redo:
//!
//! - **Normal mode**: each command (`x`, `dd`, etc.) is one transaction.
//! - **Insert mode**: everything from entering insert to pressing Esc.
//!
//! # Usage
//!
//! ```text
//! history.begin(cursor_position);
//! // perform edits on buffer, recording each one:
//! history.record_insert(pos, text);
//! history.record_delete(pos, deleted_text);
//! // finalize:
//! history.commit(cursor_position);
//! ```
//!
//! Empty transactions (no edits between begin and commit) are silently
//! discarded — they don't clutter the undo stack.

use crate::buffer::Buffer;
use crate::position::{Position, Range};

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// A single reversible buffer edit.
///
/// Each edit records the position and text involved, which is enough to
/// reconstruct both the forward and reverse operations.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// Text was inserted at `pos`. Undo = delete it. Redo = insert it.
    Insert { pos: Position, text: String },

    /// Text was deleted starting at `pos`. Undo = insert it back. Redo =
    /// delete it again.
    Delete { pos: Position, text: String },
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

/// A group of edits that undo/redo as one atomic unit.
///
/// Also tracks cursor positions so that undo restores the cursor to where it
/// was before the transaction, and redo restores it to where it was after.
#[derive(Debug, Clone)]
struct Transaction {
    edits: Vec<Edit>,
    cursor_before: Position,
    cursor_after: Position,
}

impl Transaction {
    /// Apply this transaction's edits in reverse to undo them.
    fn undo(&self, buf: &mut Buffer) {
        for edit in self.edits.iter().rev() {
            match edit {
                Edit::Insert { pos, text } => {
                    let end = end_after_insert(*pos, text);
                    buf.delete(Range::new(*pos, end));
                }
                Edit::Delete { pos, text } => {
                    buf.insert(*pos, text);
                }
            }
        }
    }

    /// Re-apply this transaction's edits in forward order.
    fn redo(&self, buf: &mut Buffer) {
        for edit in &self.edits {
            match edit {
                Edit::Insert { pos, text } => {
                    buf.insert(*pos, text);
                }
                Edit::Delete { pos, text } => {
                    let end = end_after_insert(*pos, text);
                    buf.delete(Range::new(*pos, end));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// Undo/redo history for a buffer.
///
/// Maintains two stacks: edits that can be undone and edits that can be
/// redone. New edits clear the redo stack (branching history is not
/// supported — any new edit after an undo discards the forward history).
#[derive(Debug)]
pub struct History {
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    pending: Option<Transaction>,
}

impl History {
    /// Create an empty history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending: None,
        }
    }

    /// Start a new transaction. `cursor` is the cursor position before any
    /// edits in this transaction.
    ///
    /// If a previous transaction was still pending (begin without commit),
    /// it is auto-committed first.
    pub fn begin(&mut self, cursor: Position) {
        if self.pending.is_some() {
            self.commit(cursor);
        }
        self.pending = Some(Transaction {
            edits: Vec::new(),
            cursor_before: cursor,
            cursor_after: cursor,
        });
    }

    /// Record that text was inserted at `pos`. Call this after performing
    /// the insert on the buffer.
    ///
    /// Does nothing if no transaction is pending.
    pub fn record_insert(&mut self, pos: Position, text: &str) {
        if let Some(txn) = &mut self.pending {
            txn.edits.push(Edit::Insert {
                pos,
                text: text.to_string(),
            });
        }
    }

    /// Record that text was deleted starting at `pos`. `text` is the content
    /// that was removed — capture it from the buffer before deletion.
    ///
    /// Does nothing if no transaction is pending.
    pub fn record_delete(&mut self, pos: Position, text: &str) {
        if let Some(txn) = &mut self.pending {
            txn.edits.push(Edit::Delete {
                pos,
                text: text.to_string(),
            });
        }
    }

    /// Finalize the current transaction. `cursor` is the cursor position
    /// after all edits in this transaction.
    ///
    /// Empty transactions (no edits recorded) are silently discarded.
    /// New transactions clear the redo stack.
    pub fn commit(&mut self, cursor: Position) {
        if let Some(mut txn) = self.pending.take() {
            if txn.edits.is_empty() {
                return;
            }
            txn.cursor_after = cursor;
            self.redo_stack.clear();
            self.undo_stack.push(txn);
        }
    }

    /// Undo the last transaction. Returns the cursor position to restore,
    /// or `None` if there's nothing to undo.
    pub fn undo(&mut self, buf: &mut Buffer) -> Option<Position> {
        // Auto-commit any pending transaction so it can be undone.
        if let Some(txn) = self.pending.take() {
            if !txn.edits.is_empty() {
                self.redo_stack.clear();
                self.undo_stack.push(txn);
            }
        }

        let txn = self.undo_stack.pop()?;
        txn.undo(buf);
        let cursor = txn.cursor_before;
        self.redo_stack.push(txn);
        Some(cursor)
    }

    /// Redo the last undone transaction. Returns the cursor position to
    /// restore, or `None` if there's nothing to redo.
    pub fn redo(&mut self, buf: &mut Buffer) -> Option<Position> {
        let txn = self.redo_stack.pop()?;
        txn.redo(buf);
        let cursor = txn.cursor_after;
        self.undo_stack.push(txn);
        Some(cursor)
    }

    /// True if there are transactions that can be undone.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
            || self
                .pending
                .as_ref()
                .is_some_and(|t| !t.edits.is_empty())
    }

    /// True if there are transactions that can be redone.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Number of transactions on the undo stack.
    #[must_use]
    pub fn undo_count(&self) -> usize {
        self.undo_stack.len()
    }

    /// Number of transactions on the redo stack.
    #[must_use]
    pub fn redo_count(&self) -> usize {
        self.redo_stack.len()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the position at the end of `text` if inserted at `start`.
///
/// Tracks newlines to determine the final line and column. Handles `\n`,
/// `\r\n`, and `\r` line endings correctly.
fn end_after_insert(start: Position, text: &str) -> Position {
    let mut line = start.line;
    let mut col = start.col;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                line += 1;
                col = 0;
            }
            '\r' => {
                line += 1;
                col = 0;
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            _ => {
                col += 1;
            }
        }
    }

    Position::new(line, col)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- end_after_insert ---------------------------------------------------

    #[test]
    fn end_after_insert_no_newline() {
        assert_eq!(
            end_after_insert(Position::ZERO, "hello"),
            Position::new(0, 5)
        );
    }

    #[test]
    fn end_after_insert_with_newline() {
        assert_eq!(
            end_after_insert(Position::ZERO, "hello\nworld"),
            Position::new(1, 5)
        );
    }

    #[test]
    fn end_after_insert_trailing_newline() {
        assert_eq!(
            end_after_insert(Position::ZERO, "hello\n"),
            Position::new(1, 0)
        );
    }

    #[test]
    fn end_after_insert_multiple_newlines() {
        assert_eq!(
            end_after_insert(Position::ZERO, "a\nb\nc"),
            Position::new(2, 1)
        );
    }

    #[test]
    fn end_after_insert_offset_start() {
        assert_eq!(
            end_after_insert(Position::new(3, 5), "hi"),
            Position::new(3, 7)
        );
    }

    #[test]
    fn end_after_insert_offset_with_newline() {
        assert_eq!(
            end_after_insert(Position::new(3, 5), "hi\nthere"),
            Position::new(4, 5)
        );
    }

    #[test]
    fn end_after_insert_empty() {
        assert_eq!(
            end_after_insert(Position::new(2, 3), ""),
            Position::new(2, 3)
        );
    }

    #[test]
    fn end_after_insert_crlf() {
        assert_eq!(
            end_after_insert(Position::ZERO, "hello\r\nworld"),
            Position::new(1, 5)
        );
    }

    #[test]
    fn end_after_insert_lone_cr() {
        assert_eq!(
            end_after_insert(Position::ZERO, "hello\rworld"),
            Position::new(1, 5)
        );
    }

    // -- Basic undo ---------------------------------------------------------

    #[test]
    fn undo_single_insert() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "hello");
        h.record_insert(Position::ZERO, "hello");
        h.commit(Position::new(0, 5));

        assert_eq!(buf.contents(), "hello");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "");
        assert_eq!(cursor, Position::ZERO);
    }

    #[test]
    fn undo_single_delete() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();

        let pos = Position::new(0, 4);
        h.begin(pos);
        h.record_delete(pos, "o");
        buf.delete(Range::new(pos, Position::new(0, 5)));
        h.commit(Position::new(0, 3));

        assert_eq!(buf.contents(), "hell");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hello");
        assert_eq!(cursor, pos);
    }

    // -- Basic redo ---------------------------------------------------------

    #[test]
    fn redo_after_undo() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "hello");
        h.record_insert(Position::ZERO, "hello");
        h.commit(Position::new(0, 5));

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "");

        let cursor = h.redo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hello");
        assert_eq!(cursor, Position::new(0, 5));
    }

    // -- Multiple transactions ----------------------------------------------

    #[test]
    fn undo_multiple_transactions() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "hello");
        h.record_insert(Position::ZERO, "hello");
        h.commit(Position::new(0, 5));

        h.begin(Position::new(0, 5));
        buf.insert(Position::new(0, 5), " world");
        h.record_insert(Position::new(0, 5), " world");
        h.commit(Position::new(0, 11));

        assert_eq!(buf.contents(), "hello world");

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "hello");

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "");
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "hello");
        h.record_insert(Position::ZERO, "hello");
        h.commit(Position::new(0, 5));

        h.undo(&mut buf);
        assert!(h.can_redo());

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "world");
        h.record_insert(Position::ZERO, "world");
        h.commit(Position::new(0, 5));

        assert!(!h.can_redo());
    }

    // -- Empty transactions -------------------------------------------------

    #[test]
    fn empty_transaction_not_pushed() {
        let mut h = History::new();

        h.begin(Position::ZERO);
        h.commit(Position::ZERO);

        assert!(!h.can_undo());
        assert_eq!(h.undo_count(), 0);
    }

    // -- Multi-edit transactions --------------------------------------------

    #[test]
    fn undo_multi_edit_transaction() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        // Simulate insert mode: type "hi"
        h.begin(Position::ZERO);
        buf.insert_char(Position::ZERO, 'h');
        h.record_insert(Position::ZERO, "h");
        buf.insert_char(Position::new(0, 1), 'i');
        h.record_insert(Position::new(0, 1), "i");
        h.commit(Position::new(0, 2));

        assert_eq!(buf.contents(), "hi");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "");
        assert_eq!(cursor, Position::ZERO);
    }

    #[test]
    fn undo_transaction_with_mixed_edits() {
        let mut buf = Buffer::from_text("helo");
        let mut h = History::new();

        // Insert 'l' at col 3 → "hello", then backspace → "helo", net: no change
        h.begin(Position::new(0, 3));
        buf.insert_char(Position::new(0, 3), 'l');
        h.record_insert(Position::new(0, 3), "l");

        h.record_delete(Position::new(0, 3), "l");
        buf.delete(Range::new(Position::new(0, 3), Position::new(0, 4)));

        h.commit(Position::new(0, 3));

        assert_eq!(buf.contents(), "helo");

        // Undo restores to same content but cursor_before
        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "helo");
        assert_eq!(cursor, Position::new(0, 3));
    }

    // -- Edge cases ---------------------------------------------------------

    #[test]
    fn undo_nothing() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();
        assert_eq!(h.undo(&mut buf), None);
    }

    #[test]
    fn redo_nothing() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();
        assert_eq!(h.redo(&mut buf), None);
    }

    #[test]
    fn undo_all_then_redo_all() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        let words = ["hello", " ", "world"];
        for word in &words {
            let pos = Position::new(0, buf.len_chars());
            h.begin(pos);
            buf.insert(pos, word);
            h.record_insert(pos, word);
            h.commit(Position::new(0, buf.len_chars()));
        }

        assert_eq!(buf.contents(), "hello world");

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "hello ");
        h.undo(&mut buf);
        assert_eq!(buf.contents(), "hello");
        h.undo(&mut buf);
        assert_eq!(buf.contents(), "");

        h.redo(&mut buf);
        assert_eq!(buf.contents(), "hello");
        h.redo(&mut buf);
        assert_eq!(buf.contents(), "hello ");
        h.redo(&mut buf);
        assert_eq!(buf.contents(), "hello world");
    }

    // -- Multiline edits ----------------------------------------------------

    #[test]
    fn undo_multiline_insert() {
        let mut buf = Buffer::from_text("ac");
        let mut h = History::new();

        h.begin(Position::new(0, 1));
        buf.insert(Position::new(0, 1), "b\n");
        h.record_insert(Position::new(0, 1), "b\n");
        h.commit(Position::new(1, 0));

        assert_eq!(buf.contents(), "ab\nc");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "ac");
        assert_eq!(cursor, Position::new(0, 1));
    }

    #[test]
    fn undo_multiline_delete() {
        let mut buf = Buffer::from_text("hello\nworld\nfoo");
        let mut h = History::new();

        let from = Position::new(1, 0);
        let to = Position::new(2, 0);
        let deleted = buf.slice(Range::new(from, to)).unwrap().to_string();

        h.begin(Position::new(1, 0));
        h.record_delete(from, &deleted);
        buf.delete(Range::new(from, to));
        h.commit(Position::new(1, 0));

        assert_eq!(buf.contents(), "hello\nfoo");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hello\nworld\nfoo");
        assert_eq!(cursor, Position::new(1, 0));
    }

    // -- can_undo / can_redo ------------------------------------------------

    #[test]
    fn can_undo_empty() {
        let h = History::new();
        assert!(!h.can_undo());
    }

    #[test]
    fn can_undo_after_commit() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "x");
        h.record_insert(Position::ZERO, "x");
        h.commit(Position::new(0, 1));

        assert!(h.can_undo());
    }

    #[test]
    fn can_redo_after_undo() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "x");
        h.record_insert(Position::ZERO, "x");
        h.commit(Position::new(0, 1));

        h.undo(&mut buf);
        assert!(h.can_redo());
    }

    // -- Counts -------------------------------------------------------------

    #[test]
    fn counts_track_stacks() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        assert_eq!(h.undo_count(), 0);
        assert_eq!(h.redo_count(), 0);

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "a");
        h.record_insert(Position::ZERO, "a");
        h.commit(Position::new(0, 1));

        h.begin(Position::new(0, 1));
        buf.insert(Position::new(0, 1), "b");
        h.record_insert(Position::new(0, 1), "b");
        h.commit(Position::new(0, 2));

        assert_eq!(h.undo_count(), 2);
        assert_eq!(h.redo_count(), 0);

        h.undo(&mut buf);
        assert_eq!(h.undo_count(), 1);
        assert_eq!(h.redo_count(), 1);
    }

    // -- Default ------------------------------------------------------------

    #[test]
    fn default_is_new() {
        let h = History::default();
        assert!(!h.can_undo());
        assert!(!h.can_redo());
    }

    // -- Auto-commit on begin -----------------------------------------------

    #[test]
    fn begin_auto_commits_pending() {
        let mut buf = Buffer::from_text("");
        let mut h = History::new();

        h.begin(Position::ZERO);
        buf.insert(Position::ZERO, "first");
        h.record_insert(Position::ZERO, "first");
        // No commit — start a new transaction instead.

        h.begin(Position::new(0, 5));
        buf.insert(Position::new(0, 5), "second");
        h.record_insert(Position::new(0, 5), "second");
        h.commit(Position::new(0, 11));

        assert_eq!(buf.contents(), "firstsecond");
        assert_eq!(h.undo_count(), 2);

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "first");

        h.undo(&mut buf);
        assert_eq!(buf.contents(), "");
    }

    // -- Realistic editing sequences ----------------------------------------

    #[test]
    fn simulate_x_delete_char() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();

        // x on 'e' (col 1)
        let pos = Position::new(0, 1);
        let ch = buf.char_at(pos).unwrap();

        h.begin(pos);
        h.record_delete(pos, &ch.to_string());
        buf.delete(Range::new(pos, Position::new(0, 2)));
        h.commit(pos);

        assert_eq!(buf.contents(), "hllo");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hello");
        assert_eq!(cursor, pos);

        let cursor = h.redo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hllo");
        assert_eq!(cursor, pos);
    }

    #[test]
    fn simulate_dd_delete_line() {
        let mut buf = Buffer::from_text("first\nsecond\nthird");
        let mut h = History::new();

        // dd on line 1 ("second\n")
        let from = Position::new(1, 0);
        let to = Position::new(2, 0);
        let deleted = buf.slice(Range::new(from, to)).unwrap().to_string();
        assert_eq!(deleted, "second\n");

        h.begin(Position::new(1, 0));
        h.record_delete(from, &deleted);
        buf.delete(Range::new(from, to));
        h.commit(Position::new(1, 0));

        assert_eq!(buf.contents(), "first\nthird");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "first\nsecond\nthird");
        assert_eq!(cursor, Position::new(1, 0));
    }

    #[test]
    fn simulate_insert_mode_typing() {
        let mut buf = Buffer::from_text("hllo");
        let mut h = History::new();

        // Enter insert at col 1, type 'e'
        h.begin(Position::new(0, 1));
        buf.insert_char(Position::new(0, 1), 'e');
        h.record_insert(Position::new(0, 1), "e");
        h.commit(Position::new(0, 2));

        assert_eq!(buf.contents(), "hello");

        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hllo");
        assert_eq!(cursor, Position::new(0, 1));
    }

    #[test]
    fn simulate_insert_mode_with_backspace() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();

        // Enter insert at col 5 (end)
        h.begin(Position::new(0, 5));

        // Type ' '
        buf.insert_char(Position::new(0, 5), ' ');
        h.record_insert(Position::new(0, 5), " ");

        // Type 'x'
        buf.insert_char(Position::new(0, 6), 'x');
        h.record_insert(Position::new(0, 6), "x");

        // Backspace (delete 'x')
        h.record_delete(Position::new(0, 6), "x");
        buf.delete(Range::new(Position::new(0, 6), Position::new(0, 7)));

        // Type 'w'
        buf.insert_char(Position::new(0, 6), 'w');
        h.record_insert(Position::new(0, 6), "w");

        h.commit(Position::new(0, 7));

        assert_eq!(buf.contents(), "hello w");

        // Single undo reverts entire insert session
        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "hello");
        assert_eq!(cursor, Position::new(0, 5));
    }

    #[test]
    fn simulate_o_open_line_and_type() {
        let mut buf = Buffer::from_text("first\nthird");
        let mut h = History::new();

        // 'o' opens a line below line 0
        h.begin(Position::new(0, 0));

        // Insert newline at end of line 0
        let eol = Position::new(0, 5);
        buf.insert(eol, "\n");
        h.record_insert(eol, "\n");

        // Type "second" on the new line
        buf.insert(Position::new(1, 0), "second");
        h.record_insert(Position::new(1, 0), "second");

        h.commit(Position::new(1, 6));

        assert_eq!(buf.contents(), "first\nsecond\nthird");

        // Undo removes the entire 'o' + typing
        let cursor = h.undo(&mut buf).unwrap();
        assert_eq!(buf.contents(), "first\nthird");
        assert_eq!(cursor, Position::new(0, 0));
    }

    #[test]
    fn undo_redo_undo_cycle() {
        let mut buf = Buffer::from_text("hello");
        let mut h = History::new();

        // Delete 'o'
        h.begin(Position::new(0, 4));
        h.record_delete(Position::new(0, 4), "o");
        buf.delete(Range::new(Position::new(0, 4), Position::new(0, 5)));
        h.commit(Position::new(0, 3));

        assert_eq!(buf.contents(), "hell");

        // Undo → "hello"
        h.undo(&mut buf);
        assert_eq!(buf.contents(), "hello");

        // Redo → "hell"
        h.redo(&mut buf);
        assert_eq!(buf.contents(), "hell");

        // Undo again → "hello"
        h.undo(&mut buf);
        assert_eq!(buf.contents(), "hello");
    }
}
