//! Text buffer — the fundamental unit of text storage.
//!
//! A `Buffer` wraps a [`ropey::Rope`] with ergonomic editing operations,
//! coordinate conversion between `Position` (line, col) and rope char indices,
//! file I/O, and metadata tracking (path, modified flag, line endings).
//!
//! # Design choices
//!
//! - **ropey** provides O(log n) insert/delete at any position, efficient line
//!   indexing, and battle-tested Unicode handling. We build a clean API on top
//!   rather than reimplementing text data structures.
//!
//! - **Columns are char offsets**, not byte offsets. This means column 3 of
//!   `"café"` is `'é'`, not a byte in the middle of its UTF-8 encoding. Byte
//!   offsets never leak into the public API.
//!
//! - **Line endings are detected on load** and preserved on save. Internally
//!   the rope stores whatever bytes are in the file. The `line_ending` field
//!   records the dominant style for use when saving or inserting new lines.
//!
//! - **No undo/redo here.** Edit history is a separate concern that will wrap
//!   Buffer operations with transaction tracking.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ropey::{Rope, RopeSlice};

use crate::position::{Position, Range};

// ---------------------------------------------------------------------------
// Line ending detection
// ---------------------------------------------------------------------------

/// Line ending style of a file.
///
/// Detected on load by scanning the first occurrence. Defaults to `Lf` for
/// new buffers (the Unix standard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnding {
    /// `\n` — Unix, macOS, Linux.
    Lf,
    /// `\r\n` — Windows, DOS.
    CrLf,
    /// `\r` — Classic Mac (pre-OS X). Rare but we handle it.
    Cr,
}

impl LineEnding {
    /// The string representation of this line ending.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }

    /// Detect the dominant line ending in a string by finding the first
    /// occurrence. Returns `Lf` if no line endings are found.
    #[must_use]
    pub fn detect(text: &str) -> Self {
        for (i, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                // Check if preceded by \r → CrLf.
                if i > 0 && text.as_bytes()[i - 1] == b'\r' {
                    return Self::CrLf;
                }
                return Self::Lf;
            }
            if byte == b'\r' {
                // Check if followed by \n → CrLf.
                if text.as_bytes().get(i + 1) == Some(&b'\n') {
                    return Self::CrLf;
                }
                return Self::Cr;
            }
        }
        // No line endings found — default to Lf.
        Self::Lf
    }

    /// Byte length of this line ending.
    #[inline]
    #[must_use]
    #[allow(clippy::len_without_is_empty)]
    pub const fn len(self) -> usize {
        match self {
            Self::Lf | Self::Cr => 1,
            Self::CrLf => 2,
        }
    }
}

impl fmt::Display for LineEnding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lf => f.write_str("LF"),
            Self::CrLf => f.write_str("CRLF"),
            Self::Cr => f.write_str("CR"),
        }
    }
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

/// A text buffer backed by a rope.
///
/// This is the fundamental unit of text storage in the editor. Each open file
/// (or scratch buffer) gets its own `Buffer`. The buffer tracks:
///
/// - The text content (via `ropey::Rope`)
/// - The file path (if backed by a file)
/// - Whether the content has been modified since last save
/// - The line ending style (for consistent saves)
///
/// # Coordinate system
///
/// All positions are 0-indexed `(line, col)` pairs. Columns count Unicode
/// scalar values (chars). Use [`pos_to_char_idx`](Self::pos_to_char_idx) and
/// [`char_idx_to_pos`](Self::char_idx_to_pos) for conversion to rope-native
/// char indices.
pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    modified: bool,
    line_ending: LineEnding,
}

impl Buffer {
    // -- Construction -------------------------------------------------------

    /// Create an empty buffer with no file path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            modified: false,
            line_ending: LineEnding::Lf,
        }
    }

    /// Create a buffer from a string.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self {
            line_ending: LineEnding::detect(text),
            rope: Rope::from_str(text),
            path: None,
            modified: false,
        }
    }

    /// Load a buffer from a file.
    ///
    /// Detects line endings from the file content. The buffer starts in an
    /// unmodified state.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains invalid UTF-8.
    pub fn from_file(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let line_ending = LineEnding::detect(&text);
        Ok(Self {
            rope: Rope::from_str(&text),
            path: Some(path.to_path_buf()),
            modified: false,
            line_ending,
        })
    }

    // -- Text access --------------------------------------------------------

    /// The underlying rope. Prefer the typed accessors below, but this is
    /// available when you need direct rope operations.
    #[inline]
    #[must_use]
    pub const fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Total number of lines. An empty buffer has 1 line (the empty line).
    /// A buffer ending with `\n` has a trailing empty line — this matches
    /// how editors display files.
    #[inline]
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Total character count (Unicode scalar values, not bytes).
    #[inline]
    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Total byte count.
    #[inline]
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// True when the buffer contains no text.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// Get a line by 0-indexed line number. Returns the line including its
    /// trailing line ending (if any). Returns `None` if `line >= line_count()`.
    #[inline]
    #[must_use]
    pub fn line(&self, line: usize) -> Option<RopeSlice<'_>> {
        if line < self.rope.len_lines() {
            Some(self.rope.line(line))
        } else {
            None
        }
    }

    /// Number of chars in a line **including** the trailing line ending.
    /// Returns `None` if the line doesn't exist.
    #[inline]
    #[must_use]
    pub fn line_len(&self, line: usize) -> Option<usize> {
        self.line(line).map(|l| l.len_chars())
    }

    /// Number of chars in a line **excluding** any trailing line ending
    /// (`\n`, `\r\n`, `\r`). This is the content length — the range of valid
    /// cursor columns in normal mode is `0..content_len`, and in insert mode
    /// the cursor can also sit at `content_len` (after the last char).
    ///
    /// Returns `None` if the line doesn't exist.
    #[must_use]
    pub fn line_content_len(&self, line: usize) -> Option<usize> {
        self.line(line).map(|rope_line| {
            let total = rope_line.len_chars();
            if total == 0 {
                return 0;
            }
            // Check for trailing line ending.
            let last = rope_line.char(total - 1);
            if last == '\n' {
                // Could be \r\n — check char before.
                if total >= 2 && rope_line.char(total - 2) == '\r' {
                    total - 2
                } else {
                    total - 1
                }
            } else if last == '\r' {
                total - 1
            } else {
                // Last line with no trailing newline.
                total
            }
        })
    }

    /// Get the character at a position. Returns `None` if the position is
    /// out of bounds.
    #[must_use]
    pub fn char_at(&self, pos: Position) -> Option<char> {
        self.pos_to_char_idx(pos).map(|idx| self.rope.char(idx))
    }

    /// Get a slice of text for the given range. Returns `None` if either
    /// endpoint is out of bounds.
    #[must_use]
    pub fn slice(&self, range: Range) -> Option<RopeSlice<'_>> {
        let start = self.pos_to_char_idx(range.start)?;
        let end = self.pos_to_char_idx(range.end)?;
        Some(self.rope.slice(start..end))
    }

    /// Collect all text into a `String`. Allocates — prefer `rope()` or
    /// `slice()` for zero-copy access when possible.
    #[must_use]
    pub fn contents(&self) -> String {
        self.rope.to_string()
    }

    // -- Coordinate conversion ----------------------------------------------

    /// Convert a `Position` (line, col) to an absolute char index in the rope.
    ///
    /// Returns `None` if the line is out of bounds or the column exceeds the
    /// line's total char count (including line ending). A column exactly equal
    /// to the line's char count is valid — it represents the position just past
    /// the last character (used for end-of-range and insert-mode cursors).
    #[must_use]
    pub fn pos_to_char_idx(&self, pos: Position) -> Option<usize> {
        if pos.line >= self.rope.len_lines() {
            return None;
        }
        let line_start = self.rope.line_to_char(pos.line);
        let line_len = self.rope.line(pos.line).len_chars();
        // Allow col == line_len for end-of-line positions (exclusive range
        // endpoints, insert-mode cursor). But not beyond.
        if pos.col > line_len {
            return None;
        }
        Some(line_start + pos.col)
    }

    /// Convert an absolute char index to a `Position` (line, col).
    ///
    /// Returns `None` if `char_idx > len_chars()`. An index equal to
    /// `len_chars()` returns the position just past the last character
    /// (valid for exclusive range endpoints).
    #[must_use]
    pub fn char_idx_to_pos(&self, char_idx: usize) -> Option<Position> {
        if char_idx > self.rope.len_chars() {
            return None;
        }
        let line = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(line);
        Some(Position::new(line, char_idx - line_start))
    }

    /// Clamp a position to the nearest valid position in the buffer.
    ///
    /// - If `line >= line_count()`, clamps to the last line.
    /// - If `col > line_content_len()`, clamps to `line_content_len()`.
    ///
    /// The clamped column uses content length (excluding line ending), which
    /// matches normal-mode cursor behavior. For insert-mode, the caller may
    /// want to allow one extra column.
    #[must_use]
    pub fn clamp_position(&self, pos: Position) -> Position {
        if self.is_empty() {
            return Position::ZERO;
        }

        let line = pos.line.min(self.line_count() - 1);
        let max_col = self.line_content_len(line).unwrap_or(0);
        let col = pos.col.min(max_col);

        Position::new(line, col)
    }

    // -- Editing ------------------------------------------------------------

    /// Insert text at a position.
    ///
    /// The position must be valid (see [`pos_to_char_idx`](Self::pos_to_char_idx)).
    /// After insertion, any position at or after `pos` shifts right by the
    /// length of the inserted text.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is not a valid position in the buffer.
    pub fn insert(&mut self, pos: Position, text: &str) {
        let idx = self
            .pos_to_char_idx(pos)
            .expect("insert position out of bounds");
        self.rope.insert(idx, text);
        self.modified = true;
    }

    /// Insert a single character at a position.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is not a valid position in the buffer.
    pub fn insert_char(&mut self, pos: Position, ch: char) {
        let idx = self
            .pos_to_char_idx(pos)
            .expect("insert_char position out of bounds");
        self.rope.insert_char(idx, ch);
        self.modified = true;
    }

    /// Delete the text in a range.
    ///
    /// Both `range.start` and `range.end` must be valid positions. If the
    /// range is empty, this is a no-op.
    ///
    /// # Panics
    ///
    /// Panics if either endpoint is not a valid position.
    pub fn delete(&mut self, range: Range) {
        if range.is_empty() {
            return;
        }
        let start = self
            .pos_to_char_idx(range.start)
            .expect("delete range start out of bounds");
        let end = self
            .pos_to_char_idx(range.end)
            .expect("delete range end out of bounds");
        self.rope.remove(start..end);
        self.modified = true;
    }

    /// Replace the text in a range with new text.
    ///
    /// Equivalent to a delete followed by an insert at the start position,
    /// but done as a single logical operation.
    ///
    /// # Panics
    ///
    /// Panics if either endpoint is not a valid position.
    pub fn replace(&mut self, range: Range, text: &str) {
        let start = self
            .pos_to_char_idx(range.start)
            .expect("replace range start out of bounds");
        let end = self
            .pos_to_char_idx(range.end)
            .expect("replace range end out of bounds");
        self.rope.remove(start..end);
        self.rope.insert(start, text);
        self.modified = true;
    }

    // -- Metadata -----------------------------------------------------------

    /// The file path this buffer is associated with, if any.
    #[inline]
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Set the file path for this buffer.
    #[inline]
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    /// True if the buffer has been modified since the last save (or creation).
    #[inline]
    #[must_use]
    pub const fn is_modified(&self) -> bool {
        self.modified
    }

    /// Mark the buffer as saved (not modified). Called after a successful
    /// write to disk.
    #[inline]
    pub const fn mark_saved(&mut self) {
        self.modified = false;
    }

    /// Mark the buffer as modified. Useful when external operations change
    /// buffer state outside of the normal insert/delete/replace methods.
    #[inline]
    pub const fn mark_modified(&mut self) {
        self.modified = true;
    }

    /// The detected (or configured) line ending style.
    #[inline]
    #[must_use]
    pub const fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Override the line ending style. Affects future saves but does not
    /// modify the current buffer content.
    #[inline]
    pub const fn set_line_ending(&mut self, ending: LineEnding) {
        self.line_ending = ending;
    }

    // -- File I/O -----------------------------------------------------------

    /// Save the buffer to its associated file path.
    ///
    /// Converts line endings to match the buffer's [`line_ending`](Self::line_ending)
    /// style before writing. Marks the buffer as unmodified on success.
    ///
    /// # Errors
    ///
    /// Returns an error if no path is set or the write fails.
    pub fn save(&mut self) -> io::Result<()> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer has no file path"))?
            .clone();
        self.save_as(&path)
    }

    /// Save the buffer to a specific path, updating the stored path.
    ///
    /// Converts line endings to match the buffer's [`line_ending`](Self::line_ending)
    /// style before writing. Marks the buffer as unmodified on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn save_as(&mut self, path: &Path) -> io::Result<()> {
        let content = self.text_with_line_endings();
        fs::write(path, &content)?;
        self.path = Some(path.to_path_buf());
        self.modified = false;
        Ok(())
    }

    /// Produce the full buffer text with line endings converted to the
    /// buffer's configured style.
    #[must_use]
    fn text_with_line_endings(&self) -> String {
        let raw = self.rope.to_string();

        match self.line_ending {
            LineEnding::Lf => {
                // Normalize any \r\n or lone \r to \n.
                normalize_line_endings(&raw, "\n")
            }
            LineEnding::CrLf => {
                // Normalize to \r\n.
                normalize_line_endings(&raw, "\r\n")
            }
            LineEnding::Cr => {
                // Normalize to \r.
                normalize_line_endings(&raw, "\r")
            }
        }
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("lines", &self.line_count())
            .field("chars", &self.len_chars())
            .field("modified", &self.modified)
            .field("line_ending", &self.line_ending)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize all line endings in `text` to `target`. Handles \r\n, \r, and \n
/// in any combination, converting all to the target ending.
fn normalize_line_endings(text: &str, target: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\r' {
            // \r\n or lone \r — both become one line ending.
            result.push_str(target);
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
        } else if bytes[i] == b'\n' {
            result.push_str(target);
            i += 1;
        } else {
            // Safety: we're iterating byte-by-byte but only branching on
            // ASCII bytes (\r, \n). Non-ASCII bytes are part of valid UTF-8
            // sequences that don't contain 0x0A or 0x0D, so we can safely
            // push them as chars by re-parsing.
            // Actually, let's just use the char iterator to avoid unsafe.
            break;
        }
    }

    // If we hit a non-line-ending byte, switch to char-by-char for the rest.
    if i < bytes.len() {
        // Re-do from scratch with a proper approach.
        return normalize_line_endings_chars(text, target);
    }

    result
}

/// Char-based line ending normalization. Correct for all UTF-8.
fn normalize_line_endings_chars(text: &str, target: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\r' {
            result.push_str(target);
            // Skip \n after \r (it's one line ending, not two).
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else if ch == '\n' {
            result.push_str(target);
        } else {
            result.push(ch);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- LineEnding ---------------------------------------------------------

    #[test]
    fn line_ending_detect_lf() {
        assert_eq!(LineEnding::detect("hello\nworld\n"), LineEnding::Lf);
    }

    #[test]
    fn line_ending_detect_crlf() {
        assert_eq!(LineEnding::detect("hello\r\nworld\r\n"), LineEnding::CrLf);
    }

    #[test]
    fn line_ending_detect_cr() {
        assert_eq!(LineEnding::detect("hello\rworld\r"), LineEnding::Cr);
    }

    #[test]
    fn line_ending_detect_no_endings() {
        assert_eq!(LineEnding::detect("no newlines"), LineEnding::Lf);
    }

    #[test]
    fn line_ending_detect_empty() {
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
    }

    #[test]
    fn line_ending_detect_first_wins() {
        // Mixed endings — first one determines style.
        assert_eq!(LineEnding::detect("a\nb\r\nc"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\nc"), LineEnding::CrLf);
    }

    #[test]
    fn line_ending_as_str() {
        assert_eq!(LineEnding::Lf.as_str(), "\n");
        assert_eq!(LineEnding::CrLf.as_str(), "\r\n");
        assert_eq!(LineEnding::Cr.as_str(), "\r");
    }

    #[test]
    fn line_ending_len() {
        assert_eq!(LineEnding::Lf.len(), 1);
        assert_eq!(LineEnding::CrLf.len(), 2);
        assert_eq!(LineEnding::Cr.len(), 1);
    }

    #[test]
    fn line_ending_display() {
        assert_eq!(format!("{}", LineEnding::Lf), "LF");
        assert_eq!(format!("{}", LineEnding::CrLf), "CRLF");
        assert_eq!(format!("{}", LineEnding::Cr), "CR");
    }

    // -- Buffer construction ------------------------------------------------

    #[test]
    fn new_buffer_is_empty() {
        let buf = Buffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len_chars(), 0);
        assert_eq!(buf.len_bytes(), 0);
        assert_eq!(buf.line_count(), 1); // empty buffer has one empty line
        assert!(!buf.is_modified());
        assert!(buf.path().is_none());
        assert_eq!(buf.line_ending(), LineEnding::Lf);
    }

    #[test]
    fn from_text_basic() {
        let buf = Buffer::from_text("hello\nworld\n");
        assert!(!buf.is_empty());
        assert_eq!(buf.len_chars(), 12);
        assert_eq!(buf.line_count(), 3); // "hello\n", "world\n", ""
        assert!(!buf.is_modified());
        assert_eq!(buf.line_ending(), LineEnding::Lf);
    }

    #[test]
    fn from_text_detects_crlf() {
        let buf = Buffer::from_text("hello\r\nworld\r\n");
        assert_eq!(buf.line_ending(), LineEnding::CrLf);
    }

    #[test]
    fn from_text_no_trailing_newline() {
        let buf = Buffer::from_text("hello");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.len_chars(), 5);
    }

    #[test]
    fn default_is_new() {
        let buf = Buffer::default();
        assert!(buf.is_empty());
    }

    // -- Line access --------------------------------------------------------

    #[test]
    fn line_valid() {
        let buf = Buffer::from_text("first\nsecond\nthird");
        assert_eq!(buf.line(0).unwrap().to_string(), "first\n");
        assert_eq!(buf.line(1).unwrap().to_string(), "second\n");
        assert_eq!(buf.line(2).unwrap().to_string(), "third");
    }

    #[test]
    fn line_out_of_bounds() {
        let buf = Buffer::from_text("hello\n");
        assert!(buf.line(5).is_none());
    }

    #[test]
    fn line_len_includes_newline() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(buf.line_len(0), Some(6)); // "hello\n" = 6 chars
        assert_eq!(buf.line_len(1), Some(5)); // "world" = 5 chars (no trailing \n)
    }

    #[test]
    fn line_content_len_excludes_lf() {
        let buf = Buffer::from_text("hello\nworld\n");
        assert_eq!(buf.line_content_len(0), Some(5)); // "hello"
        assert_eq!(buf.line_content_len(1), Some(5)); // "world"
        assert_eq!(buf.line_content_len(2), Some(0)); // "" (trailing empty line)
    }

    #[test]
    fn line_content_len_excludes_crlf() {
        let buf = Buffer::from_text("hello\r\nworld\r\n");
        assert_eq!(buf.line_content_len(0), Some(5)); // "hello"
        assert_eq!(buf.line_content_len(1), Some(5)); // "world"
    }

    #[test]
    fn line_content_len_excludes_cr() {
        let buf = Buffer::from_text("hello\rworld\r");
        assert_eq!(buf.line_content_len(0), Some(5));
    }

    #[test]
    fn line_content_len_no_trailing_newline() {
        let buf = Buffer::from_text("hello");
        assert_eq!(buf.line_content_len(0), Some(5)); // no newline to exclude
    }

    #[test]
    fn line_content_len_empty_line() {
        let buf = Buffer::from_text("\n\n");
        assert_eq!(buf.line_content_len(0), Some(0)); // just "\n"
        assert_eq!(buf.line_content_len(1), Some(0)); // just "\n"
    }

    #[test]
    fn line_content_len_out_of_bounds() {
        let buf = Buffer::from_text("hello");
        assert_eq!(buf.line_content_len(5), None);
    }

    // -- Character access ---------------------------------------------------

    #[test]
    fn char_at_valid() {
        let buf = Buffer::from_text("café");
        assert_eq!(buf.char_at(Position::new(0, 0)), Some('c'));
        assert_eq!(buf.char_at(Position::new(0, 3)), Some('é'));
    }

    #[test]
    fn char_at_newline() {
        let buf = Buffer::from_text("hi\nthere");
        assert_eq!(buf.char_at(Position::new(0, 2)), Some('\n'));
        assert_eq!(buf.char_at(Position::new(1, 0)), Some('t'));
    }

    #[test]
    fn char_at_out_of_bounds() {
        let buf = Buffer::from_text("hi");
        assert_eq!(buf.char_at(Position::new(0, 5)), None);
        assert_eq!(buf.char_at(Position::new(1, 0)), None);
    }

    // -- Slice access -------------------------------------------------------

    #[test]
    fn slice_single_line() {
        let buf = Buffer::from_text("hello world");
        let range = Range::new(Position::new(0, 0), Position::new(0, 5));
        assert_eq!(buf.slice(range).unwrap().to_string(), "hello");
    }

    #[test]
    fn slice_multi_line() {
        let buf = Buffer::from_text("first\nsecond\nthird");
        let range = Range::new(Position::new(0, 3), Position::new(2, 2));
        assert_eq!(buf.slice(range).unwrap().to_string(), "st\nsecond\nth");
    }

    #[test]
    fn slice_empty_range() {
        let buf = Buffer::from_text("hello");
        let range = Range::point(Position::new(0, 2));
        assert_eq!(buf.slice(range).unwrap().to_string(), "");
    }

    #[test]
    fn slice_out_of_bounds() {
        let buf = Buffer::from_text("hello");
        let range = Range::new(Position::new(0, 0), Position::new(5, 0));
        assert!(buf.slice(range).is_none());
    }

    // -- Coordinate conversion ----------------------------------------------

    #[test]
    fn pos_to_char_idx_first_line() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 0)), Some(0));
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 4)), Some(4));
    }

    #[test]
    fn pos_to_char_idx_second_line() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(buf.pos_to_char_idx(Position::new(1, 0)), Some(6));
        assert_eq!(buf.pos_to_char_idx(Position::new(1, 4)), Some(10));
    }

    #[test]
    fn pos_to_char_idx_newline_char() {
        let buf = Buffer::from_text("hello\nworld");
        // Column 5 on line 0 is the '\n' itself.
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 5)), Some(5));
    }

    #[test]
    fn pos_to_char_idx_end_of_line() {
        let buf = Buffer::from_text("hello\n");
        // Column 6 on line 0 = past the \n = valid as exclusive endpoint.
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 6)), Some(6));
    }

    #[test]
    fn pos_to_char_idx_out_of_bounds_line() {
        let buf = Buffer::from_text("hello");
        assert_eq!(buf.pos_to_char_idx(Position::new(5, 0)), None);
    }

    #[test]
    fn pos_to_char_idx_out_of_bounds_col() {
        let buf = Buffer::from_text("hi");
        // "hi" has 2 chars. col=2 is valid (end), col=3 is not.
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 2)), Some(2));
        assert_eq!(buf.pos_to_char_idx(Position::new(0, 3)), None);
    }

    #[test]
    fn char_idx_to_pos_basic() {
        let buf = Buffer::from_text("hello\nworld");
        assert_eq!(buf.char_idx_to_pos(0), Some(Position::new(0, 0)));
        assert_eq!(buf.char_idx_to_pos(5), Some(Position::new(0, 5)));
        assert_eq!(buf.char_idx_to_pos(6), Some(Position::new(1, 0)));
        assert_eq!(buf.char_idx_to_pos(10), Some(Position::new(1, 4)));
    }

    #[test]
    fn char_idx_to_pos_end_of_buffer() {
        let buf = Buffer::from_text("hello");
        // Index 5 = one past the end, valid for exclusive endpoints.
        assert_eq!(buf.char_idx_to_pos(5), Some(Position::new(0, 5)));
    }

    #[test]
    fn char_idx_to_pos_out_of_bounds() {
        let buf = Buffer::from_text("hi");
        assert_eq!(buf.char_idx_to_pos(3), None);
    }

    #[test]
    fn pos_roundtrip() {
        let buf = Buffer::from_text("hello\nworld\nfoo");
        let positions = [
            Position::new(0, 0),
            Position::new(0, 4),
            Position::new(1, 0),
            Position::new(1, 5), // the \n on line 1
            Position::new(2, 2),
        ];
        for pos in positions {
            let idx = buf.pos_to_char_idx(pos).unwrap();
            let back = buf.char_idx_to_pos(idx).unwrap();
            assert_eq!(pos, back, "roundtrip failed for {pos:?} (idx={idx})");
        }
    }

    // -- Clamp position -----------------------------------------------------

    #[test]
    fn clamp_valid_position_unchanged() {
        let buf = Buffer::from_text("hello\nworld");
        let pos = Position::new(0, 3);
        assert_eq!(buf.clamp_position(pos), pos);
    }

    #[test]
    fn clamp_line_too_high() {
        let buf = Buffer::from_text("hello\nworld");
        let clamped = buf.clamp_position(Position::new(100, 0));
        assert_eq!(clamped.line, 1);
    }

    #[test]
    fn clamp_col_too_high() {
        let buf = Buffer::from_text("hello\nworld");
        let clamped = buf.clamp_position(Position::new(0, 100));
        assert_eq!(clamped, Position::new(0, 5)); // "hello" = 5 content chars
    }

    #[test]
    fn clamp_both_too_high() {
        let buf = Buffer::from_text("hi\nbye");
        let clamped = buf.clamp_position(Position::new(50, 50));
        assert_eq!(clamped, Position::new(1, 3)); // last line "bye" = 3 chars
    }

    #[test]
    fn clamp_empty_buffer() {
        let buf = Buffer::new();
        assert_eq!(buf.clamp_position(Position::new(5, 5)), Position::ZERO);
    }

    #[test]
    fn clamp_col_at_content_boundary() {
        let buf = Buffer::from_text("hello\n");
        // Content len of line 0 is 5 ("hello"), so col=5 is valid (cursor after 'o').
        let clamped = buf.clamp_position(Position::new(0, 5));
        assert_eq!(clamped, Position::new(0, 5));
    }

    // -- Insert -------------------------------------------------------------

    #[test]
    fn insert_at_beginning() {
        let mut buf = Buffer::from_text("world");
        buf.insert(Position::ZERO, "hello ");
        assert_eq!(buf.contents(), "hello world");
        assert!(buf.is_modified());
    }

    #[test]
    fn insert_at_end() {
        let mut buf = Buffer::from_text("hello");
        buf.insert(Position::new(0, 5), " world");
        assert_eq!(buf.contents(), "hello world");
    }

    #[test]
    fn insert_in_middle() {
        let mut buf = Buffer::from_text("hllo");
        buf.insert(Position::new(0, 1), "e");
        assert_eq!(buf.contents(), "hello");
    }

    #[test]
    fn insert_newline() {
        let mut buf = Buffer::from_text("helloworld");
        buf.insert(Position::new(0, 5), "\n");
        assert_eq!(buf.contents(), "hello\nworld");
        assert_eq!(buf.line_count(), 2);
    }

    #[test]
    fn insert_multiline() {
        let mut buf = Buffer::from_text("ac");
        buf.insert(Position::new(0, 1), "b\n\n");
        assert_eq!(buf.contents(), "ab\n\nc");
        assert_eq!(buf.line_count(), 3);
    }

    #[test]
    fn insert_empty_string() {
        let mut buf = Buffer::from_text("hello");
        buf.insert(Position::new(0, 2), "");
        assert_eq!(buf.contents(), "hello");
        // Inserting empty string still marks modified (consistent behavior).
        assert!(buf.is_modified());
    }

    #[test]
    fn insert_char_method() {
        let mut buf = Buffer::from_text("hllo");
        buf.insert_char(Position::new(0, 1), 'e');
        assert_eq!(buf.contents(), "hello");
        assert!(buf.is_modified());
    }

    #[test]
    fn insert_unicode() {
        let mut buf = Buffer::from_text("caf");
        buf.insert(Position::new(0, 3), "é");
        assert_eq!(buf.contents(), "café");
    }

    #[test]
    fn insert_on_second_line() {
        let mut buf = Buffer::from_text("hello\nwrld");
        buf.insert(Position::new(1, 1), "o");
        assert_eq!(buf.contents(), "hello\nworld");
    }

    #[test]
    fn insert_sets_modified() {
        let mut buf = Buffer::from_text("hello");
        assert!(!buf.is_modified());
        buf.insert(Position::new(0, 5), "!");
        assert!(buf.is_modified());
    }

    // -- Delete -------------------------------------------------------------

    #[test]
    fn delete_single_char() {
        let mut buf = Buffer::from_text("hello");
        buf.delete(Range::new(Position::new(0, 1), Position::new(0, 2)));
        assert_eq!(buf.contents(), "hllo");
        assert!(buf.is_modified());
    }

    #[test]
    fn delete_multiple_chars() {
        let mut buf = Buffer::from_text("hello world");
        buf.delete(Range::new(Position::new(0, 5), Position::new(0, 11)));
        assert_eq!(buf.contents(), "hello");
    }

    #[test]
    fn delete_across_lines() {
        let mut buf = Buffer::from_text("hello\nworld");
        buf.delete(Range::new(Position::new(0, 3), Position::new(1, 2)));
        assert_eq!(buf.contents(), "helrld");
    }

    #[test]
    fn delete_entire_line() {
        let mut buf = Buffer::from_text("first\nsecond\nthird");
        buf.delete(Range::new(Position::new(1, 0), Position::new(2, 0)));
        assert_eq!(buf.contents(), "first\nthird");
    }

    #[test]
    fn delete_empty_range_is_noop() {
        let mut buf = Buffer::from_text("hello");
        buf.delete(Range::point(Position::new(0, 2)));
        assert_eq!(buf.contents(), "hello");
        assert!(!buf.is_modified());
    }

    #[test]
    fn delete_all() {
        let mut buf = Buffer::from_text("hello");
        buf.delete(Range::new(Position::ZERO, Position::new(0, 5)));
        assert_eq!(buf.contents(), "");
        assert!(buf.is_empty());
    }

    #[test]
    fn delete_newline() {
        let mut buf = Buffer::from_text("hello\nworld");
        buf.delete(Range::new(Position::new(0, 5), Position::new(1, 0)));
        assert_eq!(buf.contents(), "helloworld");
        assert_eq!(buf.line_count(), 1);
    }

    // -- Replace ------------------------------------------------------------

    #[test]
    fn replace_same_length() {
        let mut buf = Buffer::from_text("hello world");
        buf.replace(
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "earth",
        );
        assert_eq!(buf.contents(), "hello earth");
        assert!(buf.is_modified());
    }

    #[test]
    fn replace_shorter() {
        let mut buf = Buffer::from_text("hello beautiful world");
        buf.replace(
            Range::new(Position::new(0, 6), Position::new(0, 15)),
            "big",
        );
        assert_eq!(buf.contents(), "hello big world");
    }

    #[test]
    fn replace_longer() {
        let mut buf = Buffer::from_text("hi");
        buf.replace(
            Range::new(Position::new(0, 0), Position::new(0, 2)),
            "hello world",
        );
        assert_eq!(buf.contents(), "hello world");
    }

    #[test]
    fn replace_with_empty_is_delete() {
        let mut buf = Buffer::from_text("hello");
        buf.replace(
            Range::new(Position::new(0, 1), Position::new(0, 4)),
            "",
        );
        assert_eq!(buf.contents(), "ho");
    }

    #[test]
    fn replace_empty_range_is_insert() {
        let mut buf = Buffer::from_text("hllo");
        buf.replace(Range::point(Position::new(0, 1)), "e");
        assert_eq!(buf.contents(), "hello");
    }

    #[test]
    fn replace_across_lines() {
        let mut buf = Buffer::from_text("hello\nworld");
        buf.replace(
            Range::new(Position::new(0, 3), Position::new(1, 4)),
            "p! Goo",
        );
        assert_eq!(buf.contents(), "help! Good");
    }

    // -- Metadata -----------------------------------------------------------

    #[test]
    fn path_none_by_default() {
        let buf = Buffer::new();
        assert!(buf.path().is_none());
    }

    #[test]
    fn set_path() {
        let mut buf = Buffer::new();
        buf.set_path(PathBuf::from("/tmp/test.txt"));
        assert_eq!(buf.path(), Some(Path::new("/tmp/test.txt")));
    }

    #[test]
    fn modified_tracking() {
        let mut buf = Buffer::from_text("hello");
        assert!(!buf.is_modified());

        buf.insert(Position::new(0, 5), "!");
        assert!(buf.is_modified());

        buf.mark_saved();
        assert!(!buf.is_modified());

        buf.delete(Range::new(Position::new(0, 5), Position::new(0, 6)));
        assert!(buf.is_modified());
    }

    #[test]
    fn mark_modified_explicit() {
        let mut buf = Buffer::new();
        assert!(!buf.is_modified());
        buf.mark_modified();
        assert!(buf.is_modified());
    }

    #[test]
    fn line_ending_configurable() {
        let mut buf = Buffer::new();
        assert_eq!(buf.line_ending(), LineEnding::Lf);
        buf.set_line_ending(LineEnding::CrLf);
        assert_eq!(buf.line_ending(), LineEnding::CrLf);
    }

    // -- File I/O -----------------------------------------------------------

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("n_editor_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("roundtrip.txt");

        let mut buf = Buffer::from_text("hello\nworld\n");
        buf.save_as(&path).unwrap();

        assert!(!buf.is_modified());
        assert_eq!(buf.path(), Some(path.as_path()));

        let loaded = Buffer::from_file(&path).unwrap();
        assert_eq!(loaded.contents(), "hello\nworld\n");
        assert!(!loaded.is_modified());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn save_converts_line_endings_to_crlf() {
        let dir = std::env::temp_dir().join("n_editor_test_crlf");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crlf.txt");

        let mut buf = Buffer::from_text("hello\nworld\n");
        buf.set_line_ending(LineEnding::CrLf);
        buf.save_as(&path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "hello\r\nworld\r\n");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn save_no_path_errors() {
        let mut buf = Buffer::from_text("hello");
        let result = buf.save();
        assert!(result.is_err());
    }

    #[test]
    fn from_file_nonexistent() {
        let result = Buffer::from_file(Path::new("/nonexistent/path/file.txt"));
        assert!(result.is_err());
    }

    // -- Line ending normalization ------------------------------------------

    #[test]
    fn normalize_mixed_to_lf() {
        let result = normalize_line_endings_chars("a\r\nb\rc\n", "\n");
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn normalize_mixed_to_crlf() {
        let result = normalize_line_endings_chars("a\nb\rc\r\n", "\r\n");
        assert_eq!(result, "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn normalize_no_endings() {
        let result = normalize_line_endings_chars("hello", "\n");
        assert_eq!(result, "hello");
    }

    #[test]
    fn normalize_preserves_unicode() {
        let result = normalize_line_endings_chars("café\nnaïve\n", "\r\n");
        assert_eq!(result, "café\r\nnaïve\r\n");
    }

    // -- Unicode handling ---------------------------------------------------

    #[test]
    fn unicode_char_positions() {
        let buf = Buffer::from_text("café\nlatte");
        // "café" = 4 chars (c, a, f, é), "latte" = 5 chars
        assert_eq!(buf.line_content_len(0), Some(4));
        assert_eq!(buf.line_content_len(1), Some(5));
        assert_eq!(buf.char_at(Position::new(0, 3)), Some('é'));
        assert_eq!(buf.char_at(Position::new(1, 0)), Some('l'));
    }

    #[test]
    fn unicode_cjk() {
        let buf = Buffer::from_text("你好世界");
        assert_eq!(buf.len_chars(), 4);
        assert_eq!(buf.char_at(Position::new(0, 0)), Some('你'));
        assert_eq!(buf.char_at(Position::new(0, 3)), Some('界'));
    }

    #[test]
    fn unicode_emoji() {
        let buf = Buffer::from_text("👋🌍");
        assert_eq!(buf.len_chars(), 2);
        assert_eq!(buf.char_at(Position::new(0, 0)), Some('👋'));
        assert_eq!(buf.char_at(Position::new(0, 1)), Some('🌍'));
    }

    #[test]
    fn insert_unicode_in_middle() {
        let mut buf = Buffer::from_text("hllo");
        buf.insert(Position::new(0, 1), "é");
        assert_eq!(buf.contents(), "héllo");
        assert_eq!(buf.len_chars(), 5);
    }

    // -- Debug format -------------------------------------------------------

    #[test]
    fn buffer_debug_format() {
        let buf = Buffer::from_text("hello\nworld\n");
        let debug = format!("{buf:?}");
        assert!(debug.contains("Buffer"));
        assert!(debug.contains("lines: 3"));
        assert!(debug.contains("chars: 12"));
        assert!(debug.contains("modified: false"));
    }

    // -- Edge cases ---------------------------------------------------------

    #[test]
    fn empty_buffer_line_access() {
        let buf = Buffer::new();
        assert_eq!(buf.line_count(), 1);
        assert!(buf.line(0).is_some());
        assert_eq!(buf.line(0).unwrap().len_chars(), 0);
        assert_eq!(buf.line_content_len(0), Some(0));
    }

    #[test]
    fn single_newline() {
        let buf = Buffer::from_text("\n");
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.line_content_len(0), Some(0));
        assert_eq!(buf.line_content_len(1), Some(0));
    }

    #[test]
    fn multiple_empty_lines() {
        let buf = Buffer::from_text("\n\n\n");
        assert_eq!(buf.line_count(), 4);
        for i in 0..4 {
            assert_eq!(buf.line_content_len(i), Some(0));
        }
    }

    #[test]
    fn very_long_line() {
        let long = "a".repeat(10_000);
        let buf = Buffer::from_text(&long);
        assert_eq!(buf.len_chars(), 10_000);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.line_content_len(0), Some(10_000));
        assert_eq!(buf.char_at(Position::new(0, 9_999)), Some('a'));
    }

    #[test]
    fn insert_delete_roundtrip() {
        let mut buf = Buffer::from_text("hello world");

        // Delete "world".
        buf.delete(Range::new(Position::new(0, 6), Position::new(0, 11)));
        assert_eq!(buf.contents(), "hello ");

        // Insert it back.
        buf.insert(Position::new(0, 6), "world");
        assert_eq!(buf.contents(), "hello world");
    }

    #[test]
    fn multiple_edits_compound() {
        let mut buf = Buffer::from_text("the quick brown fox");

        // Delete "quick " → "the brown fox"
        buf.delete(Range::new(Position::new(0, 4), Position::new(0, 10)));
        assert_eq!(buf.contents(), "the brown fox");

        // Replace "brown" → "lazy" → "the lazy fox"
        buf.replace(
            Range::new(Position::new(0, 4), Position::new(0, 9)),
            "lazy",
        );
        assert_eq!(buf.contents(), "the lazy fox");

        // Insert "quick " before "lazy" → "the quick lazy fox"
        buf.insert(Position::new(0, 4), "quick ");
        assert_eq!(buf.contents(), "the quick lazy fox");
    }
}
