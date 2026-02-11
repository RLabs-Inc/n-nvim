//! Search — incremental search with match highlighting.
//!
//! Provides forward (`/`) and backward (`?`) search over a text buffer.
//! Searches are literal string matches — simple, fast, and sufficient for
//! most editing. Regex support can be layered on later.
//!
//! # Search flow
//!
//! 1. User presses `/` or `?` → editor creates a [`SearchState`]
//! 2. Each keystroke updates the input and triggers incremental search
//! 3. Enter confirms the pattern (stores it for `n`/`N` repeat)
//! 4. Escape cancels and restores the cursor to its original position
//!
//! # Match highlighting
//!
//! [`find_all`] returns all matches in a line range, used by the view layer
//! to paint match highlights on visible lines.

use crate::buffer::Buffer;
use crate::position::Position;
use crate::word::{classify, CharClass};

// ---------------------------------------------------------------------------
// Direction
// ---------------------------------------------------------------------------

/// Search direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    /// The opposite direction.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

/// A search match: start position and length in characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// Start position of the match (line, col).
    pub start: Position,
    /// Length of the match in characters.
    pub len: usize,
}

// ---------------------------------------------------------------------------
// SearchState
// ---------------------------------------------------------------------------

/// Transient state for an active search-input session.
///
/// Created when the user presses `/` or `?`. Holds the input buffer,
/// direction, and the original cursor/scroll position for cancel-restore.
pub struct SearchState {
    /// The search input being typed.
    input: String,
    /// Cursor position within the input (char offset).
    input_cursor: usize,
    /// Search direction.
    direction: SearchDirection,
    /// Cursor position before search started (for Escape restore).
    saved_pos: Position,
    /// Scroll position before search started (for Escape restore).
    saved_top_line: usize,
}

impl SearchState {
    /// Create a new search-input session.
    #[must_use]
    pub const fn new(
        direction: SearchDirection,
        saved_pos: Position,
        saved_top_line: usize,
    ) -> Self {
        Self {
            input: String::new(),
            input_cursor: 0,
            direction,
            saved_pos,
            saved_top_line,
        }
    }

    /// The current search input text.
    #[inline]
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The cursor position within the input (char offset).
    #[inline]
    #[must_use]
    pub const fn input_cursor(&self) -> usize {
        self.input_cursor
    }

    /// The search direction.
    #[inline]
    #[must_use]
    pub const fn direction(&self) -> SearchDirection {
        self.direction
    }

    /// The saved cursor position (for Escape restore).
    #[inline]
    #[must_use]
    pub const fn saved_pos(&self) -> Position {
        self.saved_pos
    }

    /// The saved scroll position (for Escape restore).
    #[inline]
    #[must_use]
    pub const fn saved_top_line(&self) -> usize {
        self.saved_top_line
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, ch: char) {
        let byte_idx = self.char_to_byte(self.input_cursor);
        self.input.insert(byte_idx, ch);
        self.input_cursor += 1;
    }

    /// Delete the character before the cursor (backspace).
    /// Returns `false` if the cursor is at position 0.
    pub fn backspace(&mut self) -> bool {
        if self.input_cursor == 0 {
            return false;
        }
        self.input_cursor -= 1;
        let byte_idx = self.char_to_byte(self.input_cursor);
        self.input.remove(byte_idx);
        true
    }

    /// Whether the input is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// The prefix character for display (`/` for forward, `?` for backward).
    #[must_use]
    pub const fn prefix(&self) -> char {
        match self.direction {
            SearchDirection::Forward => '/',
            SearchDirection::Backward => '?',
        }
    }

    /// Convert a char offset to a byte offset in the input string.
    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(byte_idx, _)| byte_idx)
    }
}

// ---------------------------------------------------------------------------
// Search functions
// ---------------------------------------------------------------------------

/// Find the next match of `pattern` searching forward from `from`.
///
/// Starts searching at (`from.line`, `from.col`) — the character at `from`
/// itself is included in the search. To skip the current position, pass
/// `(line, col + 1)`.
///
/// Wraps around: if no match is found between `from` and the end of the
/// buffer, continues from the beginning.
#[must_use]
pub fn find_forward(buf: &Buffer, pattern: &str, from: Position) -> Option<Match> {
    if pattern.is_empty() || buf.is_empty() {
        return None;
    }

    let pat_chars = pattern.chars().count();
    let line_count = buf.line_count();

    // Search from current line through all lines (wrapping around).
    for offset in 0..line_count {
        let line_idx = (from.line + offset) % line_count;
        let start_col = if offset == 0 { from.col } else { 0 };

        if let Some(m) = search_line_forward(buf, pattern, pat_chars, line_idx, start_col) {
            return Some(m);
        }
    }

    // Wrap: the loop above searched the starting line from `from.col`.
    // Check the starting line from col 0 for matches before `from.col`.
    if from.col > 0 {
        return search_line_forward(buf, pattern, pat_chars, from.line, 0);
    }

    None
}

/// Find the next match of `pattern` searching backward from `from`.
///
/// Starts searching at (`from.line`, `from.col`) — the character at `from`
/// itself is included. To skip the current position, pass `(line, col - 1)`
/// (or the end of the previous line).
///
/// Wraps around: if no match is found between `from` and the beginning of
/// the buffer, continues from the end.
#[must_use]
pub fn find_backward(buf: &Buffer, pattern: &str, from: Position) -> Option<Match> {
    if pattern.is_empty() || buf.is_empty() {
        return None;
    }

    let pat_chars = pattern.chars().count();
    let line_count = buf.line_count();

    for offset in 0..line_count {
        let line_idx = (from.line + line_count - offset) % line_count;
        // On the starting line, search up to and including from.col.
        // On other lines, search the entire line.
        let before_col = if offset == 0 { from.col } else { usize::MAX };

        if let Some(m) = search_line_backward(buf, pattern, pat_chars, line_idx, before_col) {
            return Some(m);
        }
    }

    // Wrap: the loop above searched the starting line up to `from.col`.
    // Check the starting line fully for matches after `from.col`.
    search_line_backward(buf, pattern, pat_chars, from.line, usize::MAX)
}

/// Find the next match in the given direction. Convenience wrapper over
/// [`find_forward`] and [`find_backward`].
#[must_use]
pub fn find(
    buf: &Buffer,
    pattern: &str,
    from: Position,
    direction: SearchDirection,
) -> Option<Match> {
    match direction {
        SearchDirection::Forward => find_forward(buf, pattern, from),
        SearchDirection::Backward => find_backward(buf, pattern, from),
    }
}

/// Find all matches of `pattern` in the line range `[start_line, end_line)`.
///
/// Used by the view layer to highlight all visible matches. Returns matches
/// in document order.
#[must_use]
pub fn find_all(
    buf: &Buffer,
    pattern: &str,
    start_line: usize,
    end_line: usize,
) -> Vec<Match> {
    if pattern.is_empty() {
        return Vec::new();
    }

    let pat_chars = pattern.chars().count();
    let mut matches = Vec::new();

    for line_idx in start_line..end_line.min(buf.line_count()) {
        let Some(line) = buf.line(line_idx) else {
            continue;
        };
        let line_str = line_content_string(line);

        let mut start_byte = 0;
        while start_byte < line_str.len() {
            if let Some(byte_idx) = line_str[start_byte..].find(pattern) {
                let abs_byte = start_byte + byte_idx;
                let char_col = byte_to_char(&line_str, abs_byte);
                matches.push(Match {
                    start: Position::new(line_idx, char_col),
                    len: pat_chars,
                });
                // Advance past this match (non-overlapping).
                start_byte = abs_byte + pattern.len().max(1);
            } else {
                break;
            }
        }
    }

    matches
}

/// Get the word under the cursor.
///
/// Returns the word text if the cursor is on a word or punctuation character.
/// Returns `None` if the cursor is on whitespace or the position is invalid.
#[must_use]
pub fn word_under_cursor(buf: &Buffer, pos: Position) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    let ch = buf.char_at(pos)?;
    let cls = classify(ch);
    if cls == CharClass::Blank || cls == CharClass::Newline {
        return None;
    }

    let content_len = buf.line_content_len(pos.line)?;

    // Walk backward to find word start.
    let mut start_col = pos.col;
    while start_col > 0 {
        if let Some(prev_ch) = buf.char_at(Position::new(pos.line, start_col - 1)) {
            if classify(prev_ch) != cls {
                break;
            }
            start_col -= 1;
        } else {
            break;
        }
    }

    // Walk forward to find word end (inclusive).
    let mut end_col = pos.col;
    while end_col + 1 < content_len {
        if let Some(next_ch) = buf.char_at(Position::new(pos.line, end_col + 1)) {
            if classify(next_ch) != cls {
                break;
            }
            end_col += 1;
        } else {
            break;
        }
    }

    // Extract the word.
    let range = crate::position::Range::new(
        Position::new(pos.line, start_col),
        Position::new(pos.line, end_col + 1),
    );
    buf.slice(range).map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Search forward within a single line starting at `from_col`.
fn search_line_forward(
    buf: &Buffer,
    pattern: &str,
    pat_chars: usize,
    line_idx: usize,
    from_col: usize,
) -> Option<Match> {
    let line = buf.line(line_idx)?;
    let content = line_content_string(line);

    let start_byte = char_to_byte(&content, from_col);
    if start_byte >= content.len() {
        return None;
    }

    let byte_idx = content[start_byte..].find(pattern)?;
    let abs_byte = start_byte + byte_idx;
    let char_col = byte_to_char(&content, abs_byte);
    Some(Match {
        start: Position::new(line_idx, char_col),
        len: pat_chars,
    })
}

/// Search backward within a single line, finding the last match at or before
/// `before_col`. Pass `usize::MAX` to search the entire line.
fn search_line_backward(
    buf: &Buffer,
    pattern: &str,
    pat_chars: usize,
    line_idx: usize,
    before_col: usize,
) -> Option<Match> {
    let line = buf.line(line_idx)?;
    let content = line_content_string(line);

    // Compute the byte limit: we want matches that START at or before before_col.
    let end_byte = if before_col == usize::MAX {
        content.len()
    } else {
        // Include the char at before_col + the pattern could start there.
        let col_byte = char_to_byte(&content, before_col);
        // We need to include matches starting at before_col, so search up to
        // col_byte + pattern byte length (but capped at content length).
        (col_byte + pattern.len()).min(content.len())
    };

    let search_region = &content[..end_byte];

    // Find the last occurrence using rfind.
    let byte_idx = search_region.rfind(pattern)?;
    let char_col = byte_to_char(search_region, byte_idx);

    // Verify the match starts at or before before_col.
    if before_col != usize::MAX && char_col > before_col {
        return None;
    }

    Some(Match {
        start: Position::new(line_idx, char_col),
        len: pat_chars,
    })
}

/// Extract line content as a string, excluding trailing newline characters.
fn line_content_string(line: ropey::RopeSlice<'_>) -> String {
    let s: String = line.chars().collect();
    let trimmed = s.trim_end_matches(['\n', '\r']);
    trimmed.to_string()
}

/// Convert a char offset to a byte offset in a string.
fn char_to_byte(s: &str, char_offset: usize) -> usize {
    s.char_indices()
        .nth(char_offset)
        .map_or(s.len(), |(b, _)| b)
}

/// Convert a byte offset to a char offset in a string.
fn byte_to_char(s: &str, byte_offset: usize) -> usize {
    s[..byte_offset].chars().count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- SearchDirection ----------------------------------------------------

    #[test]
    fn direction_opposite() {
        assert_eq!(SearchDirection::Forward.opposite(), SearchDirection::Backward);
        assert_eq!(SearchDirection::Backward.opposite(), SearchDirection::Forward);
    }

    // -- SearchState -------------------------------------------------------

    #[test]
    fn search_state_new() {
        let ss = SearchState::new(SearchDirection::Forward, Position::new(3, 5), 2);
        assert!(ss.is_empty());
        assert_eq!(ss.input(), "");
        assert_eq!(ss.input_cursor(), 0);
        assert_eq!(ss.direction(), SearchDirection::Forward);
        assert_eq!(ss.saved_pos(), Position::new(3, 5));
        assert_eq!(ss.saved_top_line(), 2);
        assert_eq!(ss.prefix(), '/');
    }

    #[test]
    fn search_state_backward_prefix() {
        let ss = SearchState::new(SearchDirection::Backward, Position::ZERO, 0);
        assert_eq!(ss.prefix(), '?');
    }

    #[test]
    fn search_state_insert_char() {
        let mut ss = SearchState::new(SearchDirection::Forward, Position::ZERO, 0);
        ss.insert_char('f');
        ss.insert_char('n');
        assert_eq!(ss.input(), "fn");
        assert_eq!(ss.input_cursor(), 2);
    }

    #[test]
    fn search_state_backspace() {
        let mut ss = SearchState::new(SearchDirection::Forward, Position::ZERO, 0);
        ss.insert_char('a');
        ss.insert_char('b');
        assert!(ss.backspace());
        assert_eq!(ss.input(), "a");
        assert_eq!(ss.input_cursor(), 1);
    }

    #[test]
    fn search_state_backspace_at_start() {
        let mut ss = SearchState::new(SearchDirection::Forward, Position::ZERO, 0);
        assert!(!ss.backspace());
    }

    #[test]
    fn search_state_unicode() {
        let mut ss = SearchState::new(SearchDirection::Forward, Position::ZERO, 0);
        ss.insert_char('日');
        ss.insert_char('本');
        assert_eq!(ss.input(), "日本");
        assert_eq!(ss.input_cursor(), 2);
        ss.backspace();
        assert_eq!(ss.input(), "日");
        assert_eq!(ss.input_cursor(), 1);
    }

    // -- find_forward ------------------------------------------------------

    #[test]
    fn forward_basic() {
        let buf = Buffer::from_text("hello world hello");
        let m = find_forward(&buf, "hello", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::ZERO);
        assert_eq!(m.len, 5);
    }

    #[test]
    fn forward_skip_current() {
        let buf = Buffer::from_text("hello world hello");
        // Start searching from col 1 to skip the first "hello".
        let m = find_forward(&buf, "hello", Position::new(0, 1)).unwrap();
        assert_eq!(m.start, Position::new(0, 12));
    }

    #[test]
    fn forward_multi_line() {
        let buf = Buffer::from_text("foo\nbar\nbaz");
        let m = find_forward(&buf, "bar", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::new(1, 0));
        assert_eq!(m.len, 3);
    }

    #[test]
    fn forward_wraps_around() {
        let buf = Buffer::from_text("hello world");
        // Start past "hello" — should wrap and find "hello" at col 0.
        let m = find_forward(&buf, "hello", Position::new(0, 6)).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    #[test]
    fn forward_wraps_multi_line() {
        let buf = Buffer::from_text("foo\nbar\nbaz");
        // Start on line 2 searching for "foo" — wraps to line 0.
        let m = find_forward(&buf, "foo", Position::new(2, 0)).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    #[test]
    fn forward_no_match() {
        let buf = Buffer::from_text("hello world");
        assert!(find_forward(&buf, "xyz", Position::ZERO).is_none());
    }

    #[test]
    fn forward_empty_pattern() {
        let buf = Buffer::from_text("hello");
        assert!(find_forward(&buf, "", Position::ZERO).is_none());
    }

    #[test]
    fn forward_empty_buffer() {
        let buf = Buffer::new();
        assert!(find_forward(&buf, "hello", Position::ZERO).is_none());
    }

    #[test]
    fn forward_at_end_of_line() {
        let buf = Buffer::from_text("abc\ndef");
        // Search from past end of line 0 — should find on line 1.
        let m = find_forward(&buf, "def", Position::new(0, 3)).unwrap();
        assert_eq!(m.start, Position::new(1, 0));
    }

    #[test]
    fn forward_multiple_on_same_line() {
        let buf = Buffer::from_text("abcabc");
        let m = find_forward(&buf, "abc", Position::new(0, 1)).unwrap();
        assert_eq!(m.start, Position::new(0, 3));
    }

    // -- find_backward -----------------------------------------------------

    #[test]
    fn backward_basic() {
        let buf = Buffer::from_text("hello world hello");
        let m = find_backward(&buf, "hello", Position::new(0, 16)).unwrap();
        assert_eq!(m.start, Position::new(0, 12));
    }

    #[test]
    fn backward_from_match_start() {
        let buf = Buffer::from_text("hello world hello");
        // From col 12 (start of second "hello"), searching backward
        // should find the second "hello" itself (inclusive).
        let m = find_backward(&buf, "hello", Position::new(0, 12)).unwrap();
        assert_eq!(m.start, Position::new(0, 12));
    }

    #[test]
    fn backward_skip_to_previous() {
        let buf = Buffer::from_text("hello world hello");
        // From col 11 (just before second "hello"), should find first.
        let m = find_backward(&buf, "hello", Position::new(0, 11)).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    #[test]
    fn backward_multi_line() {
        let buf = Buffer::from_text("foo\nbar\nbaz");
        let m = find_backward(&buf, "foo", Position::new(2, 0)).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    #[test]
    fn backward_wraps_around() {
        let buf = Buffer::from_text("foo\nbar\nbaz");
        // From line 0 col 0, searching backward for "baz" — wraps to line 2.
        let m = find_backward(&buf, "baz", Position::new(0, 0)).unwrap();
        // The starting line is searched first (inclusively), but "baz" isn't
        // on line 0, so it wraps: line 2, then line 1, etc.
        // Actually with offset=0, we search line 0 first. No match.
        // offset=1: line (0+3-1)%3 = 2, search full line. Found!
        assert_eq!(m.start, Position::new(2, 0));
    }

    #[test]
    fn backward_no_match() {
        let buf = Buffer::from_text("hello world");
        assert!(find_backward(&buf, "xyz", Position::new(0, 10)).is_none());
    }

    #[test]
    fn backward_empty_pattern() {
        let buf = Buffer::from_text("hello");
        assert!(find_backward(&buf, "", Position::new(0, 4)).is_none());
    }

    // -- find (direction dispatch) -----------------------------------------

    #[test]
    fn find_dispatches_forward() {
        let buf = Buffer::from_text("hello world");
        let m = find(&buf, "world", Position::ZERO, SearchDirection::Forward).unwrap();
        assert_eq!(m.start, Position::new(0, 6));
    }

    #[test]
    fn find_dispatches_backward() {
        let buf = Buffer::from_text("hello world");
        let m = find(&buf, "hello", Position::new(0, 10), SearchDirection::Backward).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    // -- find_all ----------------------------------------------------------

    #[test]
    fn find_all_basic() {
        let buf = Buffer::from_text("hello world hello");
        let matches = find_all(&buf, "hello", 0, 1);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].start, Position::ZERO);
        assert_eq!(matches[1].start, Position::new(0, 12));
    }

    #[test]
    fn find_all_multi_line() {
        let buf = Buffer::from_text("abc\nabc\nxyz\nabc");
        let matches = find_all(&buf, "abc", 0, 4);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start, Position::ZERO);
        assert_eq!(matches[1].start, Position::new(1, 0));
        assert_eq!(matches[2].start, Position::new(3, 0));
    }

    #[test]
    fn find_all_line_range() {
        let buf = Buffer::from_text("abc\nabc\nabc\nabc");
        // Only search lines 1-2.
        let matches = find_all(&buf, "abc", 1, 3);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].start, Position::new(1, 0));
        assert_eq!(matches[1].start, Position::new(2, 0));
    }

    #[test]
    fn find_all_empty_pattern() {
        let buf = Buffer::from_text("hello");
        assert!(find_all(&buf, "", 0, 1).is_empty());
    }

    #[test]
    fn find_all_no_matches() {
        let buf = Buffer::from_text("hello world");
        assert!(find_all(&buf, "xyz", 0, 1).is_empty());
    }

    #[test]
    fn find_all_multiple_per_line() {
        let buf = Buffer::from_text("aaa");
        // Non-overlapping: "a" matches at 0, 1, 2.
        let matches = find_all(&buf, "a", 0, 1);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start.col, 0);
        assert_eq!(matches[1].start.col, 1);
        assert_eq!(matches[2].start.col, 2);
    }

    #[test]
    fn find_all_non_overlapping() {
        let buf = Buffer::from_text("aaaa");
        // "aa" should match at 0 and 2 (non-overlapping).
        let matches = find_all(&buf, "aa", 0, 1);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].start.col, 0);
        assert_eq!(matches[1].start.col, 2);
    }

    // -- Unicode search ----------------------------------------------------

    #[test]
    fn forward_unicode() {
        let buf = Buffer::from_text("café latte café");
        let m = find_forward(&buf, "café", Position::new(0, 1)).unwrap();
        // Second "café" starts at char col 11.
        assert_eq!(m.start, Position::new(0, 11));
        assert_eq!(m.len, 4);
    }

    #[test]
    fn find_all_unicode() {
        let buf = Buffer::from_text("日本語で日本語");
        // "日本" appears at char cols 0 and 4.
        let matches = find_all(&buf, "日本", 0, 1);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].start.col, 0);
        assert_eq!(matches[1].start.col, 4);
    }

    #[test]
    fn backward_unicode() {
        let buf = Buffer::from_text("café latte café");
        let m = find_backward(&buf, "café", Position::new(0, 14)).unwrap();
        assert_eq!(m.start, Position::new(0, 11));
    }

    // -- word_under_cursor -------------------------------------------------

    #[test]
    fn word_under_cursor_basic() {
        let buf = Buffer::from_text("hello world");
        let word = word_under_cursor(&buf, Position::new(0, 7)).unwrap();
        assert_eq!(word, "world");
    }

    #[test]
    fn word_under_cursor_start_of_word() {
        let buf = Buffer::from_text("hello world");
        let word = word_under_cursor(&buf, Position::new(0, 6)).unwrap();
        assert_eq!(word, "world");
    }

    #[test]
    fn word_under_cursor_end_of_word() {
        let buf = Buffer::from_text("hello world");
        let word = word_under_cursor(&buf, Position::new(0, 4)).unwrap();
        assert_eq!(word, "hello");
    }

    #[test]
    fn word_under_cursor_single_char() {
        let buf = Buffer::from_text("a b c");
        let word = word_under_cursor(&buf, Position::new(0, 2)).unwrap();
        assert_eq!(word, "b");
    }

    #[test]
    fn word_under_cursor_on_whitespace() {
        let buf = Buffer::from_text("hello world");
        assert!(word_under_cursor(&buf, Position::new(0, 5)).is_none());
    }

    #[test]
    fn word_under_cursor_punctuation() {
        let buf = Buffer::from_text("foo.bar");
        // On '.': punctuation is its own word class.
        let word = word_under_cursor(&buf, Position::new(0, 3)).unwrap();
        assert_eq!(word, ".");
    }

    #[test]
    fn word_under_cursor_line_start() {
        let buf = Buffer::from_text("hello");
        let word = word_under_cursor(&buf, Position::ZERO).unwrap();
        assert_eq!(word, "hello");
    }

    #[test]
    fn word_under_cursor_unicode() {
        let buf = Buffer::from_text("café latte");
        let word = word_under_cursor(&buf, Position::new(0, 2)).unwrap();
        assert_eq!(word, "café");
    }

    #[test]
    fn word_under_cursor_empty_buffer() {
        let buf = Buffer::new();
        assert!(word_under_cursor(&buf, Position::ZERO).is_none());
    }

    // -- Helper functions --------------------------------------------------

    #[test]
    fn char_to_byte_ascii() {
        assert_eq!(char_to_byte("hello", 0), 0);
        assert_eq!(char_to_byte("hello", 3), 3);
        assert_eq!(char_to_byte("hello", 5), 5);
    }

    #[test]
    fn char_to_byte_unicode() {
        // "café" = c(1) a(1) f(1) é(2) = 5 bytes
        assert_eq!(char_to_byte("café", 0), 0);
        assert_eq!(char_to_byte("café", 3), 3); // start of 'é'
        assert_eq!(char_to_byte("café", 4), 5); // past end
    }

    #[test]
    fn byte_to_char_ascii() {
        assert_eq!(byte_to_char("hello", 0), 0);
        assert_eq!(byte_to_char("hello", 3), 3);
    }

    #[test]
    fn byte_to_char_unicode() {
        assert_eq!(byte_to_char("café", 0), 0);
        assert_eq!(byte_to_char("café", 3), 3); // start of 'é'
        assert_eq!(byte_to_char("café", 5), 4); // past 'é'
    }

    // -- Edge cases --------------------------------------------------------

    #[test]
    fn single_char_buffer() {
        let buf = Buffer::from_text("x");
        let m = find_forward(&buf, "x", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::ZERO);
        assert_eq!(m.len, 1);
    }

    #[test]
    fn pattern_at_end_of_line() {
        let buf = Buffer::from_text("hello\nworld");
        let m = find_forward(&buf, "lo", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::new(0, 3));
    }

    #[test]
    fn case_sensitive() {
        let buf = Buffer::from_text("Hello hello");
        let m = find_forward(&buf, "hello", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::new(0, 6)); // skips "Hello"
    }

    #[test]
    fn forward_same_position_wraps_full() {
        // Buffer has exactly one match at position 0. Searching from 0
        // should find it (inclusive).
        let buf = Buffer::from_text("hello world");
        let m = find_forward(&buf, "hello", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::ZERO);
    }

    #[test]
    fn backward_same_position_wraps_full() {
        // Searching backward from position 0 for "world": wraps around.
        let buf = Buffer::from_text("hello world");
        let m = find_backward(&buf, "world", Position::ZERO).unwrap();
        assert_eq!(m.start, Position::new(0, 6));
    }
}
