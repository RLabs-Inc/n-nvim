//! Text position and range types.
//!
//! All coordinates are **0-indexed**. Line 0 is the first line, column 0 is the
//! first character. Columns count Unicode scalar values (chars), not bytes or
//! grapheme clusters. This matches how `ropey` indexes text and gives O(log n)
//! access through the rope's internal tree.
//!
//! Display layers (status line, goto dialog) should convert to 1-indexed for the
//! user — that conversion never belongs here.

use std::fmt;

// ---------------------------------------------------------------------------
// Position
// ---------------------------------------------------------------------------

/// A position in a text buffer: (line, column), both 0-indexed.
///
/// `col` is the char offset from the start of the line, **not** a byte offset.
/// For the line `"café\n"`, column 3 is `'é'` and column 4 is past the last
/// visible character (the cursor-after-last-char position used in insert mode).
///
/// # Ordering
///
/// Positions are ordered lexicographically: line first, then column. This means
/// `Position { line: 0, col: 5 }` < `Position { line: 1, col: 0 }`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    /// The origin — line 0, column 0.
    pub const ZERO: Self = Self { line: 0, col: 0 };

    /// Create a new position.
    #[inline]
    #[must_use]
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }

    /// True when both line and col are zero.
    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.line == 0 && self.col == 0
    }
}

// Natural ordering: line first, then column.
impl Ord for Position {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.line
            .cmp(&other.line)
            .then(self.col.cmp(&other.col))
    }
}

impl PartialOrd for Position {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pos({}:{})", self.line, self.col)
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 1-indexed for human display, matching Vim's `line:col` status.
        write!(f, "{}:{}", self.line + 1, self.col + 1)
    }
}

// ---------------------------------------------------------------------------
// Range
// ---------------------------------------------------------------------------

/// A half-open range in a text buffer: `[start, end)`.
///
/// `start` is inclusive, `end` is exclusive. An empty range has `start == end`.
/// Ranges are always normalized so that `start <= end` — use [`Range::new`]
/// which enforces this, or [`Range::ordered`] on untrusted input.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    /// Create a range. Panics in debug if `start > end`.
    #[inline]
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        debug_assert!(
            start.line < end.line || (start.line == end.line && start.col <= end.col),
            "Range::new requires start <= end"
        );
        Self { start, end }
    }

    /// Create a range from two arbitrary positions, swapping if needed so
    /// that `start <= end`. Useful when building a range from anchor + head
    /// of a selection where the user might have dragged backwards.
    #[inline]
    #[must_use]
    pub fn ordered(a: Position, b: Position) -> Self {
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    /// A zero-width range at `Position::ZERO`.
    pub const ZERO: Self = Self {
        start: Position::ZERO,
        end: Position::ZERO,
    };

    /// A zero-width range (cursor position) at the given position.
    #[inline]
    #[must_use]
    pub const fn point(pos: Position) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }

    /// True when the range spans zero characters (`start == end`).
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.line == self.end.line && self.start.col == self.end.col
    }

    /// True when the range spans exactly one line (start and end on the same line).
    #[inline]
    #[must_use]
    pub const fn is_single_line(self) -> bool {
        self.start.line == self.end.line
    }

    /// True when the given position falls within `[start, end)`.
    #[inline]
    #[must_use]
    pub fn contains(self, pos: Position) -> bool {
        pos >= self.start && pos < self.end
    }

    /// Number of lines this range spans. A single-line range returns 1.
    /// An empty range returns 1 (it sits on one line).
    #[inline]
    #[must_use]
    pub const fn line_span(self) -> usize {
        self.end.line - self.start.line + 1
    }
}

impl fmt::Debug for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Range({}:{} .. {}:{})",
            self.start.line, self.start.col, self.end.line, self.end.col
        )
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 1-indexed for humans.
        write!(f, "{}-{}", self.start, self.end)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Position construction & constants ----------------------------------

    #[test]
    fn position_zero() {
        let p = Position::ZERO;
        assert_eq!(p.line, 0);
        assert_eq!(p.col, 0);
        assert!(p.is_zero());
    }

    #[test]
    fn position_new() {
        let p = Position::new(5, 10);
        assert_eq!(p.line, 5);
        assert_eq!(p.col, 10);
        assert!(!p.is_zero());
    }

    // -- Position ordering --------------------------------------------------

    #[test]
    fn position_ordering_same_line() {
        let a = Position::new(1, 3);
        let b = Position::new(1, 7);
        assert!(a < b);
        assert!(b > a);
    }

    #[test]
    fn position_ordering_different_lines() {
        let a = Position::new(0, 100);
        let b = Position::new(1, 0);
        assert!(a < b);
    }

    #[test]
    fn position_ordering_equal() {
        let a = Position::new(3, 3);
        let b = Position::new(3, 3);
        assert_eq!(a, b);
        assert!(a <= b);
        assert!(a >= b);
    }

    #[test]
    fn position_ord_is_consistent() {
        let positions = [
            Position::ZERO,
            Position::new(0, 1),
            Position::new(0, 100),
            Position::new(1, 0),
            Position::new(1, 1),
            Position::new(10, 0),
        ];
        for window in positions.windows(2) {
            assert!(window[0] <= window[1], "{:?} should be <= {:?}", window[0], window[1]);
        }
    }

    // -- Position display ---------------------------------------------------

    #[test]
    fn position_debug_format() {
        let p = Position::new(2, 5);
        assert_eq!(format!("{p:?}"), "Pos(2:5)");
    }

    #[test]
    fn position_display_is_1_indexed() {
        let p = Position::new(0, 0);
        assert_eq!(format!("{p}"), "1:1");

        let p = Position::new(9, 14);
        assert_eq!(format!("{p}"), "10:15");
    }

    // -- Range construction -------------------------------------------------

    #[test]
    fn range_zero() {
        let r = Range::ZERO;
        assert!(r.is_empty());
        assert_eq!(r.start, Position::ZERO);
        assert_eq!(r.end, Position::ZERO);
    }

    #[test]
    fn range_point() {
        let p = Position::new(3, 7);
        let r = Range::point(p);
        assert!(r.is_empty());
        assert_eq!(r.start, p);
        assert_eq!(r.end, p);
    }

    #[test]
    fn range_new_valid() {
        let r = Range::new(Position::new(1, 0), Position::new(1, 5));
        assert_eq!(r.start, Position::new(1, 0));
        assert_eq!(r.end, Position::new(1, 5));
    }

    #[test]
    fn range_new_same_position() {
        // start == end is valid (empty range).
        let p = Position::new(2, 3);
        let r = Range::new(p, p);
        assert!(r.is_empty());
    }

    // -- Range::ordered -----------------------------------------------------

    #[test]
    fn range_ordered_already_sorted() {
        let a = Position::new(0, 0);
        let b = Position::new(0, 5);
        let r = Range::ordered(a, b);
        assert_eq!(r.start, a);
        assert_eq!(r.end, b);
    }

    #[test]
    fn range_ordered_needs_swap() {
        let a = Position::new(5, 0);
        let b = Position::new(2, 3);
        let r = Range::ordered(a, b);
        assert_eq!(r.start, b);
        assert_eq!(r.end, a);
    }

    #[test]
    fn range_ordered_equal() {
        let p = Position::new(1, 1);
        let r = Range::ordered(p, p);
        assert!(r.is_empty());
    }

    // -- Range properties ---------------------------------------------------

    #[test]
    fn range_is_empty() {
        assert!(Range::point(Position::new(5, 5)).is_empty());
        assert!(!Range::new(Position::new(0, 0), Position::new(0, 1)).is_empty());
    }

    #[test]
    fn range_is_single_line() {
        let r = Range::new(Position::new(3, 0), Position::new(3, 10));
        assert!(r.is_single_line());

        let r = Range::new(Position::new(3, 0), Position::new(4, 0));
        assert!(!r.is_single_line());
    }

    #[test]
    fn range_line_span() {
        // Single line.
        let r = Range::new(Position::new(0, 0), Position::new(0, 5));
        assert_eq!(r.line_span(), 1);

        // Spans 3 lines (0, 1, 2).
        let r = Range::new(Position::new(0, 0), Position::new(2, 5));
        assert_eq!(r.line_span(), 3);

        // Empty range still "sits on" one line.
        assert_eq!(Range::ZERO.line_span(), 1);
    }

    // -- Range::contains ----------------------------------------------------

    #[test]
    fn range_contains_start() {
        let r = Range::new(Position::new(1, 0), Position::new(1, 5));
        assert!(r.contains(Position::new(1, 0)));
    }

    #[test]
    fn range_contains_middle() {
        let r = Range::new(Position::new(1, 0), Position::new(1, 5));
        assert!(r.contains(Position::new(1, 3)));
    }

    #[test]
    fn range_excludes_end() {
        let r = Range::new(Position::new(1, 0), Position::new(1, 5));
        assert!(!r.contains(Position::new(1, 5)));
    }

    #[test]
    fn range_excludes_before() {
        let r = Range::new(Position::new(1, 3), Position::new(1, 5));
        assert!(!r.contains(Position::new(1, 2)));
    }

    #[test]
    fn range_contains_multiline() {
        let r = Range::new(Position::new(1, 0), Position::new(3, 0));
        assert!(r.contains(Position::new(2, 50))); // middle line, any col
        assert!(!r.contains(Position::new(0, 100))); // before range
        assert!(!r.contains(Position::new(3, 0))); // at end (exclusive)
    }

    #[test]
    fn empty_range_contains_nothing() {
        let r = Range::point(Position::new(5, 5));
        assert!(!r.contains(Position::new(5, 5)));
    }

    // -- Display ------------------------------------------------------------

    #[test]
    fn range_debug_format() {
        let r = Range::new(Position::new(1, 2), Position::new(3, 4));
        assert_eq!(format!("{r:?}"), "Range(1:2 .. 3:4)");
    }

    #[test]
    fn range_display_is_1_indexed() {
        let r = Range::new(Position::new(0, 0), Position::new(2, 5));
        assert_eq!(format!("{r}"), "1:1-3:6");
    }

    // -- Equality & hashing -------------------------------------------------

    #[test]
    fn position_equality() {
        assert_eq!(Position::new(1, 2), Position::new(1, 2));
        assert_ne!(Position::new(1, 2), Position::new(1, 3));
        assert_ne!(Position::new(1, 2), Position::new(2, 2));
    }

    #[test]
    fn range_equality() {
        let r1 = Range::new(Position::new(0, 0), Position::new(1, 5));
        let r2 = Range::new(Position::new(0, 0), Position::new(1, 5));
        let r3 = Range::new(Position::new(0, 0), Position::new(1, 6));
        assert_eq!(r1, r2);
        assert_ne!(r1, r3);
    }

    #[test]
    fn position_hash_consistency() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Position::new(1, 2));
        set.insert(Position::new(1, 2)); // duplicate
        set.insert(Position::new(3, 4));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn range_hash_consistency() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let r = Range::new(Position::new(0, 0), Position::new(1, 0));
        set.insert(r);
        set.insert(r); // duplicate
        assert_eq!(set.len(), 1);
    }
}
