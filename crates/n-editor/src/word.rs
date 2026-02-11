//! Word motions — Vim-style word and WORD navigation.
//!
//! Provides the six fundamental word motions:
//!
//! | Motion | Vim key | Description |
//! |--------|---------|-------------|
//! | [`word_forward`] | `w` | Forward to start of next word |
//! | [`word_backward`] | `b` | Backward to start of previous word |
//! | [`word_end_forward`] | `e` | Forward to end of current/next word |
//! | [`big_word_forward`] | `W` | Forward to start of next WORD |
//! | [`big_word_backward`] | `B` | Backward to start of previous WORD |
//! | [`big_word_end_forward`] | `E` | Forward to end of current/next WORD |
//!
//! # Words vs WORDs
//!
//! A **word** is a sequence of word characters (letters, digits, underscore) or
//! a sequence of other non-blank characters (punctuation). Boundaries exist
//! between classes: `hello.world` contains three words (`hello`, `.`, `world`).
//!
//! A **WORD** is a sequence of non-blank characters. Only whitespace separates
//! WORDs: `hello.world` is one WORD.
//!
//! In both cases, an empty line is considered a word boundary — `w` and `b`
//! stop at empty lines (per Vim: "An empty line is also considered to be a
//! word").

use crate::buffer::Buffer;
use crate::position::Position;

// ---------------------------------------------------------------------------
// Character classification
// ---------------------------------------------------------------------------

/// Character class for word boundary detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharClass {
    /// Letters, digits, underscore.
    Word,
    /// Non-blank, non-word characters (operators, brackets, etc.).
    Punctuation,
    /// Whitespace within a line (space, tab).
    Blank,
    /// Line ending (`\n`, `\r`).
    Newline,
}

/// Classify a character for small-word motions (`w`/`b`/`e`).
pub(crate) fn classify(ch: char) -> CharClass {
    if ch == '\n' || ch == '\r' {
        CharClass::Newline
    } else if ch.is_whitespace() {
        CharClass::Blank
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

/// Classify a character for WORD motions (`W`/`B`/`E`).
/// Only blank vs non-blank matters — all non-blank chars are one class.
pub(crate) fn classify_big(ch: char) -> CharClass {
    if ch == '\n' || ch == '\r' {
        CharClass::Newline
    } else if ch.is_whitespace() {
        CharClass::Blank
    } else {
        CharClass::Word
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// `w` — forward to the start of the next word.
#[must_use]
pub fn word_forward(buf: &Buffer, pos: Position) -> Position {
    forward_start(buf, pos, classify)
}

/// `b` — backward to the start of the previous word.
#[must_use]
pub fn word_backward(buf: &Buffer, pos: Position) -> Position {
    backward_start(buf, pos, classify)
}

/// `e` — forward to the end of the current or next word.
#[must_use]
pub fn word_end_forward(buf: &Buffer, pos: Position) -> Position {
    forward_end(buf, pos, classify)
}

/// `W` — forward to the start of the next WORD.
#[must_use]
pub fn big_word_forward(buf: &Buffer, pos: Position) -> Position {
    forward_start(buf, pos, classify_big)
}

/// `B` — backward to the start of the previous WORD.
#[must_use]
pub fn big_word_backward(buf: &Buffer, pos: Position) -> Position {
    backward_start(buf, pos, classify_big)
}

/// `E` — forward to the end of the current or next WORD.
#[must_use]
pub fn big_word_end_forward(buf: &Buffer, pos: Position) -> Position {
    forward_end(buf, pos, classify_big)
}

// ---------------------------------------------------------------------------
// Core algorithms
// ---------------------------------------------------------------------------

/// Forward to the start of the next word/WORD.
///
/// 1. Skip the current token (same-class chars).
/// 2. Skip whitespace/newlines, stopping at empty lines.
/// 3. Land on the first char of the next token.
fn forward_start(
    buf: &Buffer,
    pos: Position,
    classify_fn: fn(char) -> CharClass,
) -> Position {
    let rope = buf.rope();
    let total = rope.len_chars();

    let Some(start_idx) = buf.pos_to_char_idx(pos) else {
        return pos;
    };
    if total == 0 || start_idx >= total.saturating_sub(1) {
        return pos;
    }

    let mut idx = start_idx;
    let start_class = classify_fn(rope.char(idx));

    // Phase 1: skip current token (word or punctuation group).
    if matches!(start_class, CharClass::Word | CharClass::Punctuation) {
        while idx < total && classify_fn(rope.char(idx)) == start_class {
            idx += 1;
        }
    }

    // Phase 2: skip whitespace/newlines, stopping at empty lines.
    while idx < total {
        let ch = rope.char(idx);
        match classify_fn(ch) {
            CharClass::Word | CharClass::Punctuation => break,
            CharClass::Blank => idx += 1,
            CharClass::Newline => {
                idx += 1;
                // \r\n counts as one newline.
                if ch == '\r' && idx < total && rope.char(idx) == '\n' {
                    idx += 1;
                }
                // If the next char is also a newline, we hit an empty line.
                if idx < total
                    && matches!(classify_fn(rope.char(idx)), CharClass::Newline)
                {
                    break;
                }
            }
        }
    }

    if idx >= total {
        return pos; // no next word — stay put
    }

    buf.char_idx_to_pos(idx).unwrap_or(pos)
}

/// Backward to the start of the previous word/WORD.
///
/// 1. Step back one char.
/// 2. Skip whitespace/newlines backward, stopping at empty lines.
/// 3. Skip backward through the word to its start.
fn backward_start(
    buf: &Buffer,
    pos: Position,
    classify_fn: fn(char) -> CharClass,
) -> Position {
    let rope = buf.rope();
    let total = rope.len_chars();

    let Some(start_idx) = buf.pos_to_char_idx(pos) else {
        return pos;
    };
    if start_idx == 0 || total == 0 {
        return pos;
    }

    let mut idx = start_idx - 1;

    // Phase 1: skip whitespace/newlines backward, stopping at empty lines.
    loop {
        let class = classify_fn(rope.char(idx));
        match class {
            CharClass::Word | CharClass::Punctuation => break,
            CharClass::Newline => {
                // Check if this newline sits on an empty line (content_len == 0).
                let line = rope.char_to_line(idx);
                if buf.line_content_len(line) == Some(0) {
                    // Empty line is a word boundary — stop at its start.
                    return buf
                        .char_idx_to_pos(rope.line_to_char(line))
                        .unwrap_or(pos);
                }
                if idx == 0 {
                    return buf.char_idx_to_pos(0).unwrap_or(pos);
                }
                idx -= 1;
            }
            CharClass::Blank => {
                if idx == 0 {
                    return buf.char_idx_to_pos(0).unwrap_or(pos);
                }
                idx -= 1;
            }
        }
    }

    // Phase 2: skip backward while same class to find the word start.
    let word_class = classify_fn(rope.char(idx));
    while idx > 0 && classify_fn(rope.char(idx - 1)) == word_class {
        idx -= 1;
    }

    buf.char_idx_to_pos(idx).unwrap_or(pos)
}

/// Forward to the end of the current or next word/WORD.
///
/// 1. Advance one char (so we move off the current word-end).
/// 2. Skip whitespace/newlines (no empty-line stop for `e`/`E`).
/// 3. Advance to the last char of the word.
fn forward_end(
    buf: &Buffer,
    pos: Position,
    classify_fn: fn(char) -> CharClass,
) -> Position {
    let rope = buf.rope();
    let total = rope.len_chars();

    let Some(start_idx) = buf.pos_to_char_idx(pos) else {
        return pos;
    };
    let last = total.saturating_sub(1);
    if total == 0 || start_idx >= last {
        return pos;
    }

    let mut idx = start_idx + 1;

    // Phase 1: skip whitespace/newlines.
    while idx < total {
        let class = classify_fn(rope.char(idx));
        if matches!(class, CharClass::Word | CharClass::Punctuation) {
            break;
        }
        idx += 1;
    }

    if idx >= total {
        return pos; // no word found — stay put
    }

    // Phase 2: advance to the end of this word (last char of same class).
    let word_class = classify_fn(rope.char(idx));
    while idx < last && classify_fn(rope.char(idx + 1)) == word_class {
        idx += 1;
    }

    buf.char_idx_to_pos(idx).unwrap_or(pos)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::Position;

    // Shorthand for Position.
    fn p(line: usize, col: usize) -> Position {
        Position::new(line, col)
    }

    // -- Classification -----------------------------------------------------

    #[test]
    fn classify_word_chars() {
        assert_eq!(classify('a'), CharClass::Word);
        assert_eq!(classify('Z'), CharClass::Word);
        assert_eq!(classify('0'), CharClass::Word);
        assert_eq!(classify('9'), CharClass::Word);
        assert_eq!(classify('_'), CharClass::Word);
    }

    #[test]
    fn classify_punctuation_chars() {
        assert_eq!(classify('.'), CharClass::Punctuation);
        assert_eq!(classify(','), CharClass::Punctuation);
        assert_eq!(classify('!'), CharClass::Punctuation);
        assert_eq!(classify('+'), CharClass::Punctuation);
        assert_eq!(classify('='), CharClass::Punctuation);
        assert_eq!(classify('('), CharClass::Punctuation);
    }

    #[test]
    fn classify_blank_chars() {
        assert_eq!(classify(' '), CharClass::Blank);
        assert_eq!(classify('\t'), CharClass::Blank);
    }

    #[test]
    fn classify_newline_chars() {
        assert_eq!(classify('\n'), CharClass::Newline);
        assert_eq!(classify('\r'), CharClass::Newline);
    }

    #[test]
    fn classify_unicode_letters_are_word() {
        assert_eq!(classify('é'), CharClass::Word);
        assert_eq!(classify('ñ'), CharClass::Word);
        assert_eq!(classify('中'), CharClass::Word);
        assert_eq!(classify('ü'), CharClass::Word);
    }

    #[test]
    fn classify_big_merges_punct_into_word() {
        assert_eq!(classify_big('.'), CharClass::Word);
        assert_eq!(classify_big('!'), CharClass::Word);
        assert_eq!(classify_big('a'), CharClass::Word);
        assert_eq!(classify_big(' '), CharClass::Blank);
        assert_eq!(classify_big('\n'), CharClass::Newline);
    }

    // -- word_forward (w) ---------------------------------------------------

    #[test]
    fn w_simple_two_words() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 6));
    }

    #[test]
    fn w_from_middle_of_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_forward(&buf, p(0, 2)), p(0, 6));
    }

    #[test]
    fn w_multiple_spaces() {
        let buf = Buffer::from_text("hello   world");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 8));
    }

    #[test]
    fn w_punctuation_boundary() {
        let buf = Buffer::from_text("hello.world");
        // "hello" → "." (punctuation is its own word)
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 5));
        // "." → "world"
        assert_eq!(word_forward(&buf, p(0, 5)), p(0, 6));
    }

    #[test]
    fn w_mixed_operators() {
        let buf = Buffer::from_text("x=y+z");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 1)); // x → =
        assert_eq!(word_forward(&buf, p(0, 1)), p(0, 2)); // = → y
        assert_eq!(word_forward(&buf, p(0, 2)), p(0, 3)); // y → +
        assert_eq!(word_forward(&buf, p(0, 3)), p(0, 4)); // + → z
    }

    #[test]
    fn w_across_lines() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(word_forward(&buf, p(0, 0)), p(1, 0));
    }

    #[test]
    fn w_blank_line_stop() {
        let buf = Buffer::from_text("hello\n\nworld");
        // Stops at the empty line.
        assert_eq!(word_forward(&buf, p(0, 0)), p(1, 0));
        // From empty line, continues to next word.
        assert_eq!(word_forward(&buf, p(1, 0)), p(2, 0));
    }

    #[test]
    fn w_multiple_blank_lines() {
        let buf = Buffer::from_text("hello\n\n\nworld");
        assert_eq!(word_forward(&buf, p(0, 0)), p(1, 0)); // → first blank
        assert_eq!(word_forward(&buf, p(1, 0)), p(2, 0)); // → second blank
        assert_eq!(word_forward(&buf, p(2, 0)), p(3, 0)); // → "world"
    }

    #[test]
    fn w_whitespace_only_line_not_a_stop() {
        let buf = Buffer::from_text("hello\n   \nworld");
        // Whitespace-only line is NOT an empty line — w skips over it.
        assert_eq!(word_forward(&buf, p(0, 0)), p(2, 0));
    }

    #[test]
    fn w_end_of_buffer_no_move() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn w_last_word_no_move() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_forward(&buf, p(0, 6)), p(0, 6));
    }

    #[test]
    fn w_from_whitespace() {
        let buf = Buffer::from_text("  hello");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 2));
    }

    #[test]
    fn w_from_blank_line() {
        let buf = Buffer::from_text("\nhello");
        assert_eq!(word_forward(&buf, p(0, 0)), p(1, 0));
    }

    #[test]
    fn w_empty_buffer() {
        let buf = Buffer::new();
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn w_single_char() {
        let buf = Buffer::from_text("x");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn w_three_words() {
        let buf = Buffer::from_text("one two three");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 4));
        assert_eq!(word_forward(&buf, p(0, 4)), p(0, 8));
    }

    #[test]
    fn w_tabs_as_whitespace() {
        let buf = Buffer::from_text("hello\tworld");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 6));
    }

    #[test]
    fn w_unicode_words() {
        let buf = Buffer::from_text("café naïve");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 5));
    }

    // -- word_backward (b) --------------------------------------------------

    #[test]
    fn b_simple_two_words() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_backward(&buf, p(0, 6)), p(0, 0));
    }

    #[test]
    fn b_from_middle_of_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_backward(&buf, p(0, 8)), p(0, 6));
    }

    #[test]
    fn b_multiple_spaces() {
        let buf = Buffer::from_text("hello   world");
        assert_eq!(word_backward(&buf, p(0, 8)), p(0, 0));
    }

    #[test]
    fn b_punctuation_boundary() {
        let buf = Buffer::from_text("hello.world");
        assert_eq!(word_backward(&buf, p(0, 6)), p(0, 5)); // world → .
        assert_eq!(word_backward(&buf, p(0, 5)), p(0, 0)); // . → hello
    }

    #[test]
    fn b_across_lines() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(word_backward(&buf, p(1, 0)), p(0, 0));
    }

    #[test]
    fn b_blank_line_stop() {
        let buf = Buffer::from_text("hello\n\nworld");
        assert_eq!(word_backward(&buf, p(2, 0)), p(1, 0)); // → empty line
        assert_eq!(word_backward(&buf, p(1, 0)), p(0, 0)); // → hello
    }

    #[test]
    fn b_start_of_buffer_no_move() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_backward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn b_empty_buffer() {
        let buf = Buffer::new();
        assert_eq!(word_backward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn b_three_words() {
        let buf = Buffer::from_text("one two three");
        assert_eq!(word_backward(&buf, p(0, 8)), p(0, 4));
        assert_eq!(word_backward(&buf, p(0, 4)), p(0, 0));
    }

    #[test]
    fn b_from_end_of_word() {
        let buf = Buffer::from_text("hello world");
        // From 'd' at col 10, go to start of "world" at col 6.
        assert_eq!(word_backward(&buf, p(0, 10)), p(0, 6));
    }

    #[test]
    fn b_unicode_words() {
        let buf = Buffer::from_text("café naïve");
        assert_eq!(word_backward(&buf, p(0, 5)), p(0, 0));
    }

    #[test]
    fn b_whitespace_only_line_not_a_stop() {
        let buf = Buffer::from_text("hello\n   \nworld");
        // Whitespace-only line is NOT an empty line — b skips over it.
        assert_eq!(word_backward(&buf, p(2, 0)), p(0, 0));
    }

    // -- word_end_forward (e) -----------------------------------------------

    #[test]
    fn e_simple_to_end_of_word() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 4));
    }

    #[test]
    fn e_already_at_end_goes_to_next() {
        let buf = Buffer::from_text("hello world");
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(0, 10));
    }

    #[test]
    fn e_from_middle_of_word() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_end_forward(&buf, p(0, 2)), p(0, 4));
    }

    #[test]
    fn e_punctuation_boundary() {
        let buf = Buffer::from_text("hello.world");
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 4)); // → end of hello
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(0, 5)); // → end of .
        assert_eq!(word_end_forward(&buf, p(0, 5)), p(0, 10)); // → end of world
    }

    #[test]
    fn e_across_lines() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(1, 4));
    }

    #[test]
    fn e_skips_blank_lines() {
        let buf = Buffer::from_text("hello\n\nworld");
        // e from end of "hello" skips the blank line to reach end of "world".
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(2, 4));
    }

    #[test]
    fn e_end_of_buffer_no_move() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(0, 4));
    }

    #[test]
    fn e_single_char_words() {
        let buf = Buffer::from_text("a b c");
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 2));
        assert_eq!(word_end_forward(&buf, p(0, 2)), p(0, 4));
    }

    #[test]
    fn e_empty_buffer() {
        let buf = Buffer::new();
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn e_unicode_words() {
        let buf = Buffer::from_text("café naïve");
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 3));
    }

    // -- big_word_forward (W) -----------------------------------------------

    #[test]
    fn big_w_treats_punct_as_word() {
        let buf = Buffer::from_text("hello.world next");
        // "hello.world" is one WORD.
        assert_eq!(big_word_forward(&buf, p(0, 0)), p(0, 12));
    }

    #[test]
    fn big_w_operators() {
        let buf = Buffer::from_text("x=y+z next");
        assert_eq!(big_word_forward(&buf, p(0, 0)), p(0, 6));
    }

    #[test]
    fn big_w_blank_line_stop() {
        let buf = Buffer::from_text("hello.world\n\nnext");
        assert_eq!(big_word_forward(&buf, p(0, 0)), p(1, 0));
    }

    // -- big_word_backward (B) ----------------------------------------------

    #[test]
    fn big_b_treats_punct_as_word() {
        let buf = Buffer::from_text("hello.world next");
        assert_eq!(big_word_backward(&buf, p(0, 12)), p(0, 0));
    }

    #[test]
    fn big_b_blank_line_stop() {
        let buf = Buffer::from_text("prev\n\nhello.world");
        assert_eq!(big_word_backward(&buf, p(2, 0)), p(1, 0));
    }

    // -- big_word_end_forward (E) -------------------------------------------

    #[test]
    fn big_e_treats_punct_as_word() {
        let buf = Buffer::from_text("hello.world next");
        assert_eq!(big_word_end_forward(&buf, p(0, 0)), p(0, 10));
    }

    #[test]
    fn big_e_already_at_end() {
        let buf = Buffer::from_text("hello.world next");
        assert_eq!(big_word_end_forward(&buf, p(0, 10)), p(0, 15));
    }

    // -- Edge cases ---------------------------------------------------------

    #[test]
    fn w_consecutive_punct_groups() {
        let buf = Buffer::from_text("a::b");
        assert_eq!(word_forward(&buf, p(0, 0)), p(0, 1)); // a → ::
        assert_eq!(word_forward(&buf, p(0, 1)), p(0, 3)); // :: → b
    }

    #[test]
    fn b_consecutive_punct_groups() {
        let buf = Buffer::from_text("a::b");
        assert_eq!(word_backward(&buf, p(0, 3)), p(0, 1)); // b → ::
        assert_eq!(word_backward(&buf, p(0, 1)), p(0, 0)); // :: → a
    }

    #[test]
    fn e_consecutive_punct_groups() {
        let buf = Buffer::from_text("a::b");
        assert_eq!(word_end_forward(&buf, p(0, 0)), p(0, 2)); // a → end of ::
        assert_eq!(word_end_forward(&buf, p(0, 2)), p(0, 3)); // :: → end of b
    }

    #[test]
    fn w_leading_whitespace_line() {
        let buf = Buffer::from_text("  hello  world");
        assert_eq!(word_forward(&buf, p(0, 2)), p(0, 9));
    }

    #[test]
    fn w_indented_code() {
        let buf = Buffer::from_text("    fn main() {");
        assert_eq!(word_forward(&buf, p(0, 4)), p(0, 7));  // fn → main
        assert_eq!(word_forward(&buf, p(0, 7)), p(0, 11)); // main → (
        assert_eq!(word_forward(&buf, p(0, 11)), p(0, 14)); // () → {
    }

    #[test]
    fn roundtrip_w_then_b() {
        let buf = Buffer::from_text("hello world foo");
        let start = p(0, 0);
        let mid = word_forward(&buf, start);
        assert_eq!(mid, p(0, 6));
        let back = word_backward(&buf, mid);
        assert_eq!(back, start);
    }

    #[test]
    fn w_on_last_char_of_buffer() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_forward(&buf, p(0, 4)), p(0, 4));
    }

    #[test]
    fn b_on_first_char_of_buffer() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_backward(&buf, p(0, 0)), p(0, 0));
    }

    #[test]
    fn e_on_last_char_of_buffer() {
        let buf = Buffer::from_text("hello");
        assert_eq!(word_end_forward(&buf, p(0, 4)), p(0, 4));
    }

    #[test]
    fn w_multiline_three_lines() {
        let buf = Buffer::from_text("one\ntwo\nthree");
        assert_eq!(word_forward(&buf, p(0, 0)), p(1, 0));
        assert_eq!(word_forward(&buf, p(1, 0)), p(2, 0));
    }

    #[test]
    fn b_multiline_three_lines() {
        let buf = Buffer::from_text("one\ntwo\nthree");
        assert_eq!(word_backward(&buf, p(2, 0)), p(1, 0));
        assert_eq!(word_backward(&buf, p(1, 0)), p(0, 0));
    }
}
