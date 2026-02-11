//! Text objects — Vim-style text selection by structure.
//!
//! Text objects define regions of text by structure rather than cursor motion.
//! Combined with operators (`d`, `c`, `y`), they form Vim's composable grammar:
//!
//! ```text
//! operator + text-object = action
//! d        + iw          = delete inner word
//! c        + i"          = change inside quotes
//! y        + a(          = yank around parentheses
//! ```
//!
//! Each function takes a buffer and a cursor position, returning
//! `Option<Range>` — the half-open `[start, end)` range of the text object.
//! Returns `None` when the object cannot be found (e.g., no enclosing brackets).
//!
//! # Supported text objects
//!
//! | Inner    | Around   | Description                     |
//! |----------|----------|---------------------------------|
//! | `iw`     | `aw`     | word (letters, digits, `_`)     |
//! | `iW`     | `aW`     | WORD (non-blank characters)     |
//! | `i"`     | `a"`     | double-quoted string            |
//! | `i'`     | `a'`     | single-quoted string            |
//! | `` i` `` | `` a` `` | backtick-quoted string          |
//! | `i(`     | `a(`     | parenthesized block             |
//! | `i[`     | `a[`     | square-bracketed block          |
//! | `i{`     | `a{`     | curly-braced block              |
//! | `i<`     | `a<`     | angle-bracketed block           |

use crate::buffer::Buffer;
use crate::position::{Position, Range};
use crate::word::{classify, classify_big, CharClass};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a char index to a Position, handling end-of-buffer.
///
/// Unlike `Buffer::char_idx_to_pos`, this handles `idx >= len_chars()` by
/// mapping to the position after the last character. Needed for half-open
/// range endpoints.
fn idx_to_pos(buf: &Buffer, idx: usize) -> Position {
    let rope = buf.rope();
    let total = rope.len_chars();
    if total == 0 {
        return Position::ZERO;
    }
    if idx >= total {
        let last_line = rope.len_lines().saturating_sub(1);
        let line_start = rope.line_to_char(last_line);
        Position::new(last_line, total - line_start)
    } else {
        buf.char_idx_to_pos(idx).unwrap_or(Position::ZERO)
    }
}

// ---------------------------------------------------------------------------
// Word objects
// ---------------------------------------------------------------------------

/// `iw` — inner word.
///
/// Selects the word under the cursor. If the cursor is on whitespace, selects
/// the whitespace run. Uses small-word classification (letters/digits/`_` are
/// one class, punctuation is another).
#[must_use]
pub fn inner_word(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_word_impl(buf, pos, classify)
}

/// `aw` — a word.
///
/// Selects the word under the cursor plus surrounding whitespace. If the cursor
/// is on a word, includes trailing whitespace (or leading if at end of line).
/// If the cursor is on whitespace, includes the following word.
#[must_use]
pub fn a_word(buf: &Buffer, pos: Position) -> Option<Range> {
    a_word_impl(buf, pos, classify)
}

/// `iW` — inner WORD.
///
/// Like `iw` but uses WORD boundaries (only whitespace separates WORDs).
#[must_use]
pub fn inner_big_word(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_word_impl(buf, pos, classify_big)
}

/// `aW` — a WORD.
///
/// Like `aw` but uses WORD boundaries.
#[must_use]
pub fn a_big_word(buf: &Buffer, pos: Position) -> Option<Range> {
    a_word_impl(buf, pos, classify_big)
}

/// Core algorithm for inner word/WORD.
///
/// Finds the run of same-class characters around the cursor position.
/// For Word/Punctuation: expands to the full run.
/// For Blank: expands whitespace but stops at newlines.
/// For Newline: selects just the newline character(s).
fn inner_word_impl(
    buf: &Buffer,
    pos: Position,
    classify_fn: fn(char) -> CharClass,
) -> Option<Range> {
    let rope = buf.rope();
    let total = rope.len_chars();
    let idx = buf.pos_to_char_idx(pos)?;
    if total == 0 || idx >= total {
        return None;
    }

    let ch = rope.char(idx);
    let class = classify_fn(ch);

    let (start, end) = match class {
        CharClass::Word | CharClass::Punctuation => {
            let mut s = idx;
            while s > 0 && classify_fn(rope.char(s - 1)) == class {
                s -= 1;
            }
            let mut e = idx + 1;
            while e < total && classify_fn(rope.char(e)) == class {
                e += 1;
            }
            (s, e)
        }
        CharClass::Blank => {
            let mut s = idx;
            while s > 0 && classify_fn(rope.char(s - 1)) == CharClass::Blank {
                s -= 1;
            }
            let mut e = idx + 1;
            while e < total && classify_fn(rope.char(e)) == CharClass::Blank {
                e += 1;
            }
            (s, e)
        }
        CharClass::Newline => {
            let mut e = idx + 1;
            // Handle \r\n as a single newline.
            if ch == '\r' && e < total && rope.char(e) == '\n' {
                e += 1;
            }
            (idx, e)
        }
    };

    Some(Range::new(idx_to_pos(buf, start), idx_to_pos(buf, end)))
}

/// Core algorithm for a word/WORD.
///
/// Extends the inner word to include surrounding whitespace:
/// - On a word/punct: tries trailing whitespace first, then leading.
/// - On whitespace: includes the following word.
/// - On newline: same as inner (just the newline).
fn a_word_impl(
    buf: &Buffer,
    pos: Position,
    classify_fn: fn(char) -> CharClass,
) -> Option<Range> {
    let rope = buf.rope();
    let total = rope.len_chars();
    let inner = inner_word_impl(buf, pos, classify_fn)?;

    let start_idx = buf.pos_to_char_idx(inner.start)?;
    let end_idx = buf.pos_to_char_idx(inner.end).unwrap_or(total);

    let idx = buf.pos_to_char_idx(pos)?;
    let class = classify_fn(rope.char(idx));

    match class {
        CharClass::Word | CharClass::Punctuation => {
            // Try trailing whitespace first.
            let mut new_end = end_idx;
            while new_end < total && classify_fn(rope.char(new_end)) == CharClass::Blank {
                new_end += 1;
            }
            if new_end > end_idx {
                return Some(Range::new(inner.start, idx_to_pos(buf, new_end)));
            }

            // No trailing whitespace — try leading whitespace.
            let mut new_start = start_idx;
            while new_start > 0
                && classify_fn(rope.char(new_start - 1)) == CharClass::Blank
            {
                new_start -= 1;
            }
            if new_start < start_idx {
                return Some(Range::new(idx_to_pos(buf, new_start), inner.end));
            }

            // No surrounding whitespace at all — return inner.
            Some(inner)
        }
        CharClass::Blank => {
            // On whitespace: include the following word.
            let mut new_end = end_idx;
            if new_end < total {
                let next_class = classify_fn(rope.char(new_end));
                if matches!(next_class, CharClass::Word | CharClass::Punctuation) {
                    while new_end < total && classify_fn(rope.char(new_end)) == next_class {
                        new_end += 1;
                    }
                }
            }
            Some(Range::new(inner.start, idx_to_pos(buf, new_end)))
        }
        CharClass::Newline => Some(inner),
    }
}

// ---------------------------------------------------------------------------
// Quote objects
// ---------------------------------------------------------------------------

/// `i"` — inner double quote.
#[must_use]
pub fn inner_double_quote(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_quote(buf, pos, '"')
}

/// `a"` — a double quote (including the quotes).
#[must_use]
pub fn a_double_quote(buf: &Buffer, pos: Position) -> Option<Range> {
    a_quote(buf, pos, '"')
}

/// `i'` — inner single quote.
#[must_use]
pub fn inner_single_quote(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_quote(buf, pos, '\'')
}

/// `a'` — a single quote (including the quotes).
#[must_use]
pub fn a_single_quote(buf: &Buffer, pos: Position) -> Option<Range> {
    a_quote(buf, pos, '\'')
}

/// `` i` `` — inner backtick quote.
#[must_use]
pub fn inner_backtick(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_quote(buf, pos, '`')
}

/// `` a` `` — a backtick quote (including the backticks).
#[must_use]
pub fn a_backtick(buf: &Buffer, pos: Position) -> Option<Range> {
    a_quote(buf, pos, '`')
}

/// Inner quote — text between quotes (excluding the quotes themselves).
fn inner_quote(buf: &Buffer, pos: Position, quote: char) -> Option<Range> {
    let (open_col, close_col) = find_quote_pair(buf, pos, quote)?;
    let start = Position::new(pos.line, open_col + 1);
    let end = Position::new(pos.line, close_col);
    if start > end {
        return Some(Range::point(start));
    }
    Some(Range::new(start, end))
}

/// Around quote — text including the quotes.
fn a_quote(buf: &Buffer, pos: Position, quote: char) -> Option<Range> {
    let (open_col, close_col) = find_quote_pair(buf, pos, quote)?;
    Some(Range::new(
        Position::new(pos.line, open_col),
        Position::new(pos.line, close_col + 1),
    ))
}

/// Find the quote pair on the current line that contains (or follows) the cursor.
///
/// Quotes are paired left-to-right: the 1st and 2nd form a pair, the 3rd and
/// 4th form another, etc. If the cursor is inside a pair, returns it. If the
/// cursor is before or between pairs, returns the next pair forward (Vim 7.4+
/// behavior).
fn find_quote_pair(buf: &Buffer, pos: Position, quote: char) -> Option<(usize, usize)> {
    let line = buf.line(pos.line)?;

    // Collect column offsets of all quote characters on this line.
    let mut quotes = Vec::new();
    for (i, ch) in line.chars().enumerate() {
        if ch == '\n' || ch == '\r' {
            break;
        }
        if ch == quote {
            quotes.push(i);
        }
    }

    // Need at least one pair.
    if quotes.len() < 2 {
        return None;
    }

    let col = pos.col;

    // Find the pair containing the cursor.
    for pair in quotes.chunks(2) {
        if pair.len() == 2 {
            let (open, close) = (pair[0], pair[1]);
            if col >= open && col <= close {
                return Some((open, close));
            }
        }
    }

    // Cursor is outside all pairs — find the next pair forward.
    for pair in quotes.chunks(2) {
        if pair.len() == 2 && pair[0] > col {
            return Some((pair[0], pair[1]));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Bracket objects
// ---------------------------------------------------------------------------

/// `i(` / `i)` — inner parentheses.
#[must_use]
pub fn inner_paren(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_bracket(buf, pos, '(', ')')
}

/// `a(` / `a)` — around parentheses.
#[must_use]
pub fn a_paren(buf: &Buffer, pos: Position) -> Option<Range> {
    a_bracket(buf, pos, '(', ')')
}

/// `i[` / `i]` — inner square brackets.
#[must_use]
pub fn inner_square(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_bracket(buf, pos, '[', ']')
}

/// `a[` / `a]` — around square brackets.
#[must_use]
pub fn a_square(buf: &Buffer, pos: Position) -> Option<Range> {
    a_bracket(buf, pos, '[', ']')
}

/// `i{` / `i}` — inner curly braces.
#[must_use]
pub fn inner_curly(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_bracket(buf, pos, '{', '}')
}

/// `a{` / `a}` — around curly braces.
#[must_use]
pub fn a_curly(buf: &Buffer, pos: Position) -> Option<Range> {
    a_bracket(buf, pos, '{', '}')
}

/// `i<` / `i>` — inner angle brackets.
#[must_use]
pub fn inner_angle(buf: &Buffer, pos: Position) -> Option<Range> {
    inner_bracket(buf, pos, '<', '>')
}

/// `a<` / `a>` — around angle brackets.
#[must_use]
pub fn a_angle(buf: &Buffer, pos: Position) -> Option<Range> {
    a_bracket(buf, pos, '<', '>')
}

/// Inner bracket — text between matching brackets (excluding brackets).
fn inner_bracket(buf: &Buffer, pos: Position, open: char, close: char) -> Option<Range> {
    let (open_idx, close_idx) = find_bracket_pair(buf, pos, open, close)?;
    let start = open_idx + 1;
    let end = close_idx;
    if start >= end {
        let p = idx_to_pos(buf, start);
        return Some(Range::point(p));
    }
    Some(Range::new(idx_to_pos(buf, start), idx_to_pos(buf, end)))
}

/// Around bracket — text including the brackets themselves.
fn a_bracket(buf: &Buffer, pos: Position, open: char, close: char) -> Option<Range> {
    let (open_idx, close_idx) = find_bracket_pair(buf, pos, open, close)?;
    Some(Range::new(
        idx_to_pos(buf, open_idx),
        idx_to_pos(buf, close_idx + 1),
    ))
}

/// Find the matching bracket pair containing the cursor.
///
/// Handles nesting and works across multiple lines. Returns the char indices
/// of the opening and closing brackets: `(open_idx, close_idx)`.
fn find_bracket_pair(
    buf: &Buffer,
    pos: Position,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let rope = buf.rope();
    let total = rope.len_chars();
    let cursor_idx = buf.pos_to_char_idx(pos)?;
    if total == 0 || cursor_idx >= total {
        return None;
    }

    let cursor_char = rope.char(cursor_idx);

    // Cursor is on the opening bracket — search forward for the close.
    if cursor_char == open {
        let close_idx = find_closing(rope, cursor_idx, total, open, close)?;
        return Some((cursor_idx, close_idx));
    }

    // Cursor is on the closing bracket — search backward for the open.
    if cursor_char == close {
        let open_idx = find_opening(rope, cursor_idx, open, close)?;
        return Some((open_idx, cursor_idx));
    }

    // Cursor is between brackets — search backward for the open, then
    // forward from that open for the matching close.
    let open_idx = find_opening(rope, cursor_idx, open, close)?;
    let close_idx = find_closing(rope, open_idx, total, open, close)?;

    // Verify the cursor is actually inside this pair.
    if cursor_idx > open_idx && cursor_idx < close_idx {
        Some((open_idx, close_idx))
    } else {
        None
    }
}

/// Search backward from `start` for an unmatched opening bracket.
///
/// Tracks nesting: each close bracket increases depth, each open bracket
/// decreases it. When depth reaches 0 at an open bracket, that's the match.
fn find_opening(rope: &ropey::Rope, start: usize, open: char, close: char) -> Option<usize> {
    let mut depth: usize = 0;
    let mut i = start;

    loop {
        if i == 0 {
            if rope.char(0) == open && depth == 0 {
                return Some(0);
            }
            return None;
        }
        i -= 1;

        let ch = rope.char(i);
        if ch == close {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
}

/// Search forward from `start` for the matching closing bracket.
///
/// Tracks nesting: each open bracket increases depth, each close bracket
/// decreases it. When depth reaches 0 at a close bracket, that's the match.
fn find_closing(
    rope: &ropey::Rope,
    start: usize,
    total: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth: usize = 0;
    for i in (start + 1)..total {
        let ch = rope.char(i);
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: usize, col: usize) -> Position {
        Position::new(line, col)
    }

    fn r(sl: usize, sc: usize, el: usize, ec: usize) -> Range {
        Range::new(p(sl, sc), p(el, ec))
    }

    // == Word objects ========================================================

    // -- inner_word (iw) ----------------------------------------------------

    #[test]
    fn iw_on_word_start() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(inner_word(&buf, p(0, 0)), Some(r(0, 0, 0, 5)));
    }

    #[test]
    fn iw_middle_of_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(inner_word(&buf, p(0, 2)), Some(r(0, 0, 0, 5)));
    }

    #[test]
    fn iw_end_of_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(inner_word(&buf, p(0, 4)), Some(r(0, 0, 0, 5)));
    }

    #[test]
    fn iw_second_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(inner_word(&buf, p(0, 6)), Some(r(0, 6, 0, 11)));
    }

    #[test]
    fn iw_on_punctuation() {
        let buf = Buffer::from_text("hello.world");
        assert_eq!(inner_word(&buf, p(0, 5)), Some(r(0, 5, 0, 6)));
    }

    #[test]
    fn iw_on_punctuation_run() {
        let buf = Buffer::from_text("a::b");
        assert_eq!(inner_word(&buf, p(0, 1)), Some(r(0, 1, 0, 3)));
    }

    #[test]
    fn iw_on_whitespace() {
        let buf = Buffer::from_text("hello   world");
        assert_eq!(inner_word(&buf, p(0, 6)), Some(r(0, 5, 0, 8)));
    }

    #[test]
    fn iw_single_char_word() {
        let buf = Buffer::from_text("a b c");
        assert_eq!(inner_word(&buf, p(0, 0)), Some(r(0, 0, 0, 1)));
        assert_eq!(inner_word(&buf, p(0, 2)), Some(r(0, 2, 0, 3)));
    }

    #[test]
    fn iw_on_empty_line() {
        let buf = Buffer::from_text("hello\n\nworld");
        // Empty line: cursor on '\n', selects just the newline.
        assert_eq!(inner_word(&buf, p(1, 0)), Some(r(1, 0, 2, 0)));
    }

    #[test]
    fn iw_empty_buffer() {
        let buf = Buffer::new();
        assert_eq!(inner_word(&buf, p(0, 0)), None);
    }

    #[test]
    fn iw_single_char() {
        let buf = Buffer::from_text("x");
        assert_eq!(inner_word(&buf, p(0, 0)), Some(r(0, 0, 0, 1)));
    }

    #[test]
    fn iw_underscore_in_word() {
        let buf = Buffer::from_text("foo_bar baz");
        assert_eq!(inner_word(&buf, p(0, 2)), Some(r(0, 0, 0, 7)));
    }

    #[test]
    fn iw_unicode_word() {
        let buf = Buffer::from_text("café naïve");
        assert_eq!(inner_word(&buf, p(0, 0)), Some(r(0, 0, 0, 4)));
    }

    // -- a_word (aw) --------------------------------------------------------

    #[test]
    fn aw_trailing_whitespace() {
        let buf = Buffer::from_text("hello world");
        // "hello" + trailing space = [0, 6)
        assert_eq!(a_word(&buf, p(0, 2)), Some(r(0, 0, 0, 6)));
    }

    #[test]
    fn aw_leading_whitespace() {
        let buf = Buffer::from_text("hello world");
        // "world" is the last word — no trailing space, include leading.
        assert_eq!(a_word(&buf, p(0, 7)), Some(r(0, 5, 0, 11)));
    }

    #[test]
    fn aw_no_surrounding_whitespace() {
        let buf = Buffer::from_text("hello");
        // Single word, no whitespace — same as iw.
        assert_eq!(a_word(&buf, p(0, 2)), Some(r(0, 0, 0, 5)));
    }

    #[test]
    fn aw_on_whitespace_includes_next_word() {
        let buf = Buffer::from_text("hello   world");
        // On whitespace: includes whitespace + "world".
        assert_eq!(a_word(&buf, p(0, 6)), Some(r(0, 5, 0, 13)));
    }

    #[test]
    fn aw_multiple_spaces() {
        let buf = Buffer::from_text("one   two   three");
        // "two" at [6, 9). Trailing spaces [9, 12).
        assert_eq!(a_word(&buf, p(0, 7)), Some(r(0, 6, 0, 12)));
    }

    #[test]
    fn aw_on_punctuation() {
        let buf = Buffer::from_text("hello.world");
        // Punctuation "." — no surrounding whitespace.
        assert_eq!(a_word(&buf, p(0, 5)), Some(r(0, 5, 0, 6)));
    }

    // -- inner_big_word (iW) ------------------------------------------------

    #[test]
    fn iw_big_includes_punctuation() {
        let buf = Buffer::from_text("hello.world next");
        // "hello.world" is one WORD.
        assert_eq!(inner_big_word(&buf, p(0, 3)), Some(r(0, 0, 0, 11)));
    }

    #[test]
    fn iw_big_operators_merged() {
        let buf = Buffer::from_text("x=y+z next");
        assert_eq!(inner_big_word(&buf, p(0, 2)), Some(r(0, 0, 0, 5)));
    }

    // -- a_big_word (aW) ----------------------------------------------------

    #[test]
    fn aw_big_trailing_space() {
        let buf = Buffer::from_text("hello.world next");
        assert_eq!(a_big_word(&buf, p(0, 3)), Some(r(0, 0, 0, 12)));
    }

    // == Quote objects =======================================================

    // -- inner_double_quote (i") --------------------------------------------

    #[test]
    fn iq_simple() {
        let buf = Buffer::from_text("say \"hello\" now");
        assert_eq!(inner_double_quote(&buf, p(0, 6)), Some(r(0, 5, 0, 10)));
    }

    #[test]
    fn iq_cursor_on_open_quote() {
        let buf = Buffer::from_text("say \"hello\" now");
        assert_eq!(inner_double_quote(&buf, p(0, 4)), Some(r(0, 5, 0, 10)));
    }

    #[test]
    fn iq_cursor_on_close_quote() {
        let buf = Buffer::from_text("say \"hello\" now");
        assert_eq!(inner_double_quote(&buf, p(0, 10)), Some(r(0, 5, 0, 10)));
    }

    #[test]
    fn iq_cursor_before_quotes() {
        let buf = Buffer::from_text("say \"hello\" now");
        // Before the pair — selects the first pair forward.
        assert_eq!(inner_double_quote(&buf, p(0, 1)), Some(r(0, 5, 0, 10)));
    }

    #[test]
    fn iq_empty_quotes() {
        let buf = Buffer::from_text("say \"\" now");
        // Empty quotes: range is a point between them.
        assert_eq!(
            inner_double_quote(&buf, p(0, 4)),
            Some(Range::point(p(0, 5)))
        );
    }

    #[test]
    fn iq_no_quotes() {
        let buf = Buffer::from_text("no quotes here");
        assert_eq!(inner_double_quote(&buf, p(0, 5)), None);
    }

    #[test]
    fn iq_single_quote_only() {
        let buf = Buffer::from_text("just one \" here");
        assert_eq!(inner_double_quote(&buf, p(0, 5)), None);
    }

    #[test]
    fn iq_multiple_pairs() {
        let buf = Buffer::from_text("\"aa\" \"bb\"");
        // Cursor in first pair.
        assert_eq!(inner_double_quote(&buf, p(0, 1)), Some(r(0, 1, 0, 3)));
        // Cursor in second pair.
        assert_eq!(inner_double_quote(&buf, p(0, 6)), Some(r(0, 6, 0, 8)));
    }

    #[test]
    fn iq_cursor_between_pairs() {
        let buf = Buffer::from_text("\"aa\" x \"bb\"");
        // Cursor on 'x' between pairs — selects the next pair forward.
        // Quotes at: 0, 3, 7, 10. Pairs: (0,3), (7,10).
        // inner of (7,10) = [8, 10).
        assert_eq!(inner_double_quote(&buf, p(0, 5)), Some(r(0, 8, 0, 10)));
    }

    // -- a_double_quote (a") ------------------------------------------------

    #[test]
    fn aq_simple() {
        let buf = Buffer::from_text("say \"hello\" now");
        assert_eq!(a_double_quote(&buf, p(0, 6)), Some(r(0, 4, 0, 11)));
    }

    #[test]
    fn aq_empty_quotes() {
        let buf = Buffer::from_text("say \"\" now");
        assert_eq!(a_double_quote(&buf, p(0, 4)), Some(r(0, 4, 0, 6)));
    }

    // -- single quotes (i'/a') ----------------------------------------------

    #[test]
    fn isq_simple() {
        let buf = Buffer::from_text("say 'hello' now");
        // Quotes at cols 4 and 10. Inner = [5, 10).
        assert_eq!(inner_single_quote(&buf, p(0, 6)), Some(r(0, 5, 0, 10)));
    }

    // -- backtick quotes (i`/a`) --------------------------------------------

    #[test]
    fn ibq_simple() {
        let buf = Buffer::from_text("use `code` here");
        assert_eq!(inner_backtick(&buf, p(0, 6)), Some(r(0, 5, 0, 9)));
    }

    #[test]
    fn abq_simple() {
        let buf = Buffer::from_text("use `code` here");
        assert_eq!(a_backtick(&buf, p(0, 6)), Some(r(0, 4, 0, 10)));
    }

    // == Bracket objects =====================================================

    // -- inner_paren (i() ---------------------------------------------------

    #[test]
    fn ip_simple() {
        let buf = Buffer::from_text("f(hello)");
        assert_eq!(inner_paren(&buf, p(0, 3)), Some(r(0, 2, 0, 7)));
    }

    #[test]
    fn ip_cursor_on_open() {
        let buf = Buffer::from_text("(hello)");
        assert_eq!(inner_paren(&buf, p(0, 0)), Some(r(0, 1, 0, 6)));
    }

    #[test]
    fn ip_cursor_on_close() {
        let buf = Buffer::from_text("(hello)");
        assert_eq!(inner_paren(&buf, p(0, 6)), Some(r(0, 1, 0, 6)));
    }

    #[test]
    fn ip_empty() {
        let buf = Buffer::from_text("f()");
        assert_eq!(inner_paren(&buf, p(0, 1)), Some(Range::point(p(0, 2))));
    }

    #[test]
    fn ip_nested_inner() {
        let buf = Buffer::from_text("f(a(b)c)");
        // Cursor on 'b' at col 4 — selects inner pair.
        assert_eq!(inner_paren(&buf, p(0, 4)), Some(r(0, 4, 0, 5)));
    }

    #[test]
    fn ip_nested_outer() {
        let buf = Buffer::from_text("f(a(b)c)");
        // Cursor on 'a' at col 2 — selects outer pair.
        assert_eq!(inner_paren(&buf, p(0, 2)), Some(r(0, 2, 0, 7)));
    }

    #[test]
    fn ip_multiline() {
        let buf = Buffer::from_text("f(\n  hello\n)");
        assert_eq!(inner_paren(&buf, p(1, 2)), Some(r(0, 2, 2, 0)));
    }

    #[test]
    fn ip_no_match() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(inner_paren(&buf, p(0, 3)), None);
    }

    #[test]
    fn ip_unmatched_open() {
        let buf = Buffer::from_text("f(hello");
        assert_eq!(inner_paren(&buf, p(0, 3)), None);
    }

    #[test]
    fn ip_unmatched_close() {
        let buf = Buffer::from_text("hello)");
        assert_eq!(inner_paren(&buf, p(0, 3)), None);
    }

    // -- a_paren (a() -------------------------------------------------------

    #[test]
    fn ap_simple() {
        let buf = Buffer::from_text("f(hello)");
        assert_eq!(a_paren(&buf, p(0, 3)), Some(r(0, 1, 0, 8)));
    }

    #[test]
    fn ap_nested_outer() {
        let buf = Buffer::from_text("f(a(b)c)");
        // Cursor on 'a' — outer pair.
        assert_eq!(a_paren(&buf, p(0, 2)), Some(r(0, 1, 0, 8)));
    }

    #[test]
    fn ap_multiline() {
        let buf = Buffer::from_text("f(\n  hello\n)");
        assert_eq!(a_paren(&buf, p(1, 2)), Some(r(0, 1, 2, 1)));
    }

    // -- square brackets (i[/a[) -------------------------------------------

    #[test]
    fn isq_brackets() {
        let buf = Buffer::from_text("arr[42]");
        assert_eq!(inner_square(&buf, p(0, 4)), Some(r(0, 4, 0, 6)));
    }

    #[test]
    fn asq_brackets() {
        let buf = Buffer::from_text("arr[42]");
        assert_eq!(a_square(&buf, p(0, 4)), Some(r(0, 3, 0, 7)));
    }

    // -- curly braces (i{/a{) -----------------------------------------------

    #[test]
    fn ic_simple() {
        let buf = Buffer::from_text("{ body }");
        assert_eq!(inner_curly(&buf, p(0, 3)), Some(r(0, 1, 0, 7)));
    }

    #[test]
    fn ac_simple() {
        let buf = Buffer::from_text("{ body }");
        assert_eq!(a_curly(&buf, p(0, 3)), Some(r(0, 0, 0, 8)));
    }

    #[test]
    fn ic_multiline() {
        let buf = Buffer::from_text("fn main() {\n    body\n}");
        assert_eq!(inner_curly(&buf, p(1, 4)), Some(r(0, 11, 2, 0)));
    }

    // -- angle brackets (i</a<) --------------------------------------------

    #[test]
    fn ia_simple() {
        let buf = Buffer::from_text("Vec<i32>");
        assert_eq!(inner_angle(&buf, p(0, 5)), Some(r(0, 4, 0, 7)));
    }

    #[test]
    fn aa_simple() {
        let buf = Buffer::from_text("Vec<i32>");
        assert_eq!(a_angle(&buf, p(0, 5)), Some(r(0, 3, 0, 8)));
    }

    #[test]
    fn ia_nested() {
        let buf = Buffer::from_text("Vec<Option<i32>>");
        // Cursor on 'i' at col 11 — inner angle of inner pair.
        assert_eq!(inner_angle(&buf, p(0, 11)), Some(r(0, 11, 0, 14)));
        // Cursor on 'O' at col 4 — inner angle of outer pair.
        assert_eq!(inner_angle(&buf, p(0, 4)), Some(r(0, 4, 0, 15)));
    }

    // == Edge cases ==========================================================

    #[test]
    fn iw_at_buffer_end() {
        let buf = Buffer::from_text("hello");
        assert_eq!(inner_word(&buf, p(0, 4)), Some(r(0, 0, 0, 5)));
    }

    #[test]
    fn iq_on_different_line() {
        let buf = Buffer::from_text("first line\n\"second\" line");
        // Quotes are on line 1 only.
        assert_eq!(inner_double_quote(&buf, p(0, 3)), None);
        assert_eq!(inner_double_quote(&buf, p(1, 3)), Some(r(1, 1, 1, 7)));
    }

    #[test]
    fn ip_deeply_nested() {
        let buf = Buffer::from_text("(a(b(c)d)e)");
        // Cursor on 'c' at col 5 — innermost.
        assert_eq!(inner_paren(&buf, p(0, 5)), Some(r(0, 5, 0, 6)));
        // Cursor on 'b' at col 3 — middle.
        assert_eq!(inner_paren(&buf, p(0, 3)), Some(r(0, 3, 0, 8)));
        // Cursor on 'a' at col 1 — outermost.
        assert_eq!(inner_paren(&buf, p(0, 1)), Some(r(0, 1, 0, 10)));
    }

    #[test]
    fn aw_word_at_start_of_line() {
        let buf = Buffer::from_text("hello world");
        // First word has trailing space.
        assert_eq!(a_word(&buf, p(0, 0)), Some(r(0, 0, 0, 6)));
    }

    #[test]
    fn aw_word_at_end_of_line() {
        let buf = Buffer::from_text("hello world");
        // Last word: no trailing space, include leading space.
        assert_eq!(a_word(&buf, p(0, 6)), Some(r(0, 5, 0, 11)));
    }

    #[test]
    fn iw_whitespace_single_space() {
        let buf = Buffer::from_text("a b");
        assert_eq!(inner_word(&buf, p(0, 1)), Some(r(0, 1, 0, 2)));
    }

    #[test]
    fn ip_cursor_on_nested_close() {
        let buf = Buffer::from_text("f(a(b)c)");
        // Cursor on ')' at col 5 — the inner close paren.
        assert_eq!(inner_paren(&buf, p(0, 5)), Some(r(0, 4, 0, 5)));
    }

    #[test]
    fn ip_cursor_on_outer_close() {
        let buf = Buffer::from_text("f(a(b)c)");
        // Cursor on ')' at col 7 — the outer close paren.
        assert_eq!(inner_paren(&buf, p(0, 7)), Some(r(0, 2, 0, 7)));
    }
}
