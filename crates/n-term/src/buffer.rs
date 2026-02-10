// SPDX-License-Identifier: MIT
//
// FrameBuffer — the 2D cell grid that everything paints to.
//
// Every character position on screen is a cell in this buffer. The editor
// core, split tree, floating windows, and all UI elements paint here.
// The diff renderer then compares this frame against the previous one
// and emits minimal ANSI escape sequences for the changes.
//
// Design:
//
//   - Flat `Vec<Cell>` with row-major indexing for cache efficiency.
//     A row's cells are contiguous in memory, so left-to-right iteration
//     (which the renderer does) is a linear scan.
//
//   - All paint operations accept an optional `ClipRect` for overflow:hidden.
//     UI elements paint freely; clipping constrains them to their region.
//
//   - Transparent backgrounds composite via `Color::resolve_over()` in
//     linear sRGB — physically correct blending with no dark seam artifacts.
//
//   - Wide characters (CJK, some emoji) occupy two columns. The first cell
//     holds the codepoint; the second is a continuation cell (ch = 0).
//     Paint methods handle continuation cell creation and wide-char cleanup.
//
// Memory:
//
//   200×50 terminal = 10,000 cells × 16 bytes = 160 KB per buffer.
//   Even 4K terminals (480×120 = 57,600 cells = 900 KB) are trivial.
//   Two buffers for double-buffering: ~1.8 MB. No concern.

use unicode_width::UnicodeWidthChar;

use crate::cell::{Attr, Cell, UnderlineStyle};
use crate::color::{CellColor, Color};

// ─── ClipRect ───────────────────────────────────────────────────────────────────

/// A clipping rectangle for overflow handling.
///
/// Used by paint methods to constrain drawing to a region. Coordinates are
/// signed to support scroll offsets (a UI element can be partially scrolled
/// off-screen with negative x/y).
///
/// # Examples
///
/// ```
/// use n_term::buffer::ClipRect;
///
/// let clip = ClipRect::new(10, 5, 80, 24);
/// assert!(clip.contains(10, 5));    // top-left corner: inside
/// assert!(clip.contains(89, 28));   // bottom-right corner: inside
/// assert!(!clip.contains(9, 5));    // left of bounds: outside
/// assert!(!clip.contains(90, 5));   // right of bounds: outside
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    /// Left edge (may be negative for scrolled content).
    pub x: i32,
    /// Top edge (may be negative for scrolled content).
    pub y: i32,
    /// Width in columns.
    pub width: u16,
    /// Height in rows.
    pub height: u16,
}

impl ClipRect {
    /// Create a clipping rectangle with signed coordinates.
    #[inline]
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u16, height: u16) -> Self {
        Self { x, y, width, height }
    }

    /// Create from unsigned screen-space coordinates.
    #[inline]
    #[must_use]
    pub const fn from_unsigned(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x: x as i32,
            y: y as i32,
            width,
            height,
        }
    }

    /// Right edge (exclusive): `x + width`.
    #[inline]
    #[must_use]
    pub const fn right(self) -> i32 {
        self.x + self.width as i32
    }

    /// Bottom edge (exclusive): `y + height`.
    #[inline]
    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.y + self.height as i32
    }

    /// Whether this rectangle has zero area.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Whether a screen-space point is inside this rectangle.
    ///
    /// Screen coordinates are `u16` (terminal positions are never negative).
    /// The clip rect may have negative x/y from scroll offsets.
    #[inline]
    #[must_use]
    pub fn contains(self, px: u16, py: u16) -> bool {
        let px = i32::from(px);
        let py = i32::from(py);
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Compute the intersection of two rectangles.
    ///
    /// Returns `None` if they don't overlap.
    #[must_use]
    pub fn intersect(self, other: Self) -> Option<Self> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());

        if x2 > x1 && y2 > y1 {
            // Safe: both differences are positive (x2 > x1, y2 > y1) and
            // bounded by input widths/heights which are u16.
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Some(Self {
                x: x1,
                y: y1,
                width: (x2 - x1) as u16,
                height: (y2 - y1) as u16,
            })
        } else {
            None
        }
    }
}

// ─── FrameBuffer ────────────────────────────────────────────────────────────────

/// A 2D buffer of terminal cells — the canvas everything paints to.
///
/// Flat `Vec<Cell>` with row-major indexing: `index = y * width + x`.
/// Rows are contiguous in memory for cache-efficient left-to-right scanning
/// during rendering.
///
/// # Examples
///
/// ```
/// use n_term::buffer::FrameBuffer;
/// use n_term::cell::Cell;
///
/// let mut buf = FrameBuffer::new(80, 24);
/// assert_eq!(buf.width(), 80);
/// assert_eq!(buf.height(), 24);
///
/// buf.set(5, 3, Cell::new('X'));
/// assert_eq!(buf.get(5, 3).unwrap().character(), Some('X'));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct FrameBuffer {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl FrameBuffer {
    // ─── Construction ────────────────────────────────────────────────────

    /// Create a buffer filled with empty cells (space, default colors).
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        let size = usize::from(width) * usize::from(height);
        Self {
            width,
            height,
            cells: vec![Cell::EMPTY; size],
        }
    }

    /// Create a buffer with a specific background color.
    ///
    /// Takes [`CellColor`] (not [`Color`]) because there's no existing cell
    /// to composite against at creation time. If you have a `Color`, call
    /// [`.resolve()`](Color::resolve) first.
    #[must_use]
    pub fn with_bg(width: u16, height: u16, bg: CellColor) -> Self {
        let size = usize::from(width) * usize::from(height);
        Self {
            width,
            height,
            cells: vec![Cell::EMPTY.with_bg(bg); size],
        }
    }

    // ─── Accessors ───────────────────────────────────────────────────────

    /// Buffer width in columns.
    #[inline]
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// Buffer height in rows.
    #[inline]
    #[must_use]
    pub const fn height(&self) -> u16 {
        self.height
    }

    /// Total number of cells (`width × height`).
    #[inline]
    #[must_use]
    pub fn total_cells(&self) -> usize {
        self.cells.len()
    }

    /// The full buffer bounds as a [`ClipRect`].
    #[inline]
    #[must_use]
    pub const fn bounds(&self) -> ClipRect {
        ClipRect::new(0, 0, self.width, self.height)
    }

    /// Whether `(x, y)` is within the buffer.
    #[inline]
    #[must_use]
    pub const fn in_bounds(&self, x: u16, y: u16) -> bool {
        x < self.width && y < self.height
    }

    /// Convert `(x, y)` to a flat index.
    #[inline]
    const fn index(&self, x: u16, y: u16) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// Get a cell reference, or `None` if out of bounds.
    #[inline]
    #[must_use]
    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        if self.in_bounds(x, y) {
            Some(&self.cells[self.index(x, y)])
        } else {
            None
        }
    }

    /// Get a mutable cell reference, or `None` if out of bounds.
    #[inline]
    pub fn get_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if self.in_bounds(x, y) {
            let idx = self.index(x, y);
            Some(&mut self.cells[idx])
        } else {
            None
        }
    }

    /// The raw cell slice (for the diff renderer's hot loop).
    #[inline]
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// A single row as a slice. Returns `None` if `y` is out of bounds.
    #[inline]
    #[must_use]
    pub fn row(&self, y: u16) -> Option<&[Cell]> {
        if y < self.height {
            let start = self.index(0, y);
            Some(&self.cells[start..start + usize::from(self.width)])
        } else {
            None
        }
    }

    /// A single mutable row slice. Returns `None` if `y` is out of bounds.
    #[inline]
    pub fn row_mut(&mut self, y: u16) -> Option<&mut [Cell]> {
        if y < self.height {
            let start = self.index(0, y);
            let w = usize::from(self.width);
            Some(&mut self.cells[start..start + w])
        } else {
            None
        }
    }

    /// Iterate cells with their `(x, y)` coordinates.
    #[allow(clippy::cast_possible_truncation)]
    pub fn iter(&self) -> impl Iterator<Item = (u16, u16, &Cell)> {
        let w = usize::from(self.width).max(1); // max(1) prevents div-by-zero in dead code
        self.cells.iter().enumerate().map(move |(i, cell)| {
            // Safe truncation: x < width (u16) and y < height (u16).
            let x = (i % w) as u16;
            let y = (i / w) as u16;
            (x, y, cell)
        })
    }

    // ─── Clear & Resize ──────────────────────────────────────────────────

    /// Clear the buffer to empty cells (space, default colors, no attrs).
    pub fn clear(&mut self) {
        self.cells.fill(Cell::EMPTY);
    }

    /// Clear with a specific background color.
    pub fn clear_with_bg(&mut self, bg: CellColor) {
        self.cells.fill(Cell::EMPTY.with_bg(bg));
    }

    /// Resize the buffer, clearing all content.
    ///
    /// After resize, all cells are empty (space, default colors).
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        let size = usize::from(width) * usize::from(height);
        self.cells.clear();
        self.cells.resize(size, Cell::EMPTY);
    }

    // ─── Direct Cell Access ──────────────────────────────────────────────

    /// Write a cell directly to the buffer.
    ///
    /// No compositing, no clipping, no wide-char cleanup. Just a
    /// bounds-checked write. Use this for buffer-to-buffer copies
    /// or when cells are already fully resolved.
    ///
    /// Returns `true` if the position was in bounds.
    #[inline]
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        let idx = self.index(x, y);
        self.cells[idx] = cell;
        true
    }

    // ─── Wide Character Cleanup ──────────────────────────────────────────

    /// Break any wide character that touches position `(x, y)`.
    ///
    /// - If `(x, y)` is a continuation cell, replaces the owner at `(x-1)`
    ///   with a space.
    /// - If the cell after `(x, y)` is a continuation, it was part of a
    ///   wide char starting here — clear the orphaned continuation.
    fn break_wide_char_at(&mut self, x: u16, y: u16) {
        let idx = self.index(x, y);

        // If this cell is a continuation, break the wide char that owns it.
        if self.cells[idx].is_continuation() && x > 0 {
            let prev = self.index(x - 1, y);
            self.cells[prev].ch = u32::from(b' ');
        }

        // If the next cell is a continuation, this cell was a wide char
        // start — clear the orphaned continuation.
        if x + 1 < self.width {
            let next = self.index(x + 1, y);
            if self.cells[next].is_continuation() {
                self.cells[next] = Cell::EMPTY;
            }
        }
    }

    // ─── Paint — with compositing ───────────────────────────────────────

    /// Paint a cell with transparency compositing and clipping.
    ///
    /// - `fg` is converted to [`CellColor`] directly (terminals don't support
    ///   foreground transparency).
    /// - `bg` is composited over the existing cell's background via
    ///   [`Color::resolve_over`] in linear sRGB. Opaque backgrounds skip
    ///   the blend (fast path).
    ///
    /// Wide-char safety: if the target position belongs to a wide character
    /// (either as a continuation cell or as the start), the wide character
    /// is broken and cleaned up.
    ///
    /// Returns `true` if the cell was painted (in bounds and not clipped).
    #[allow(clippy::too_many_arguments, clippy::similar_names)]
    pub fn paint_cell(
        &mut self,
        x: u16,
        y: u16,
        ch: char,
        fg: Color,
        bg: Color,
        attrs: Attr,
        underline: UnderlineStyle,
        clip: Option<&ClipRect>,
    ) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        if let Some(clip) = clip {
            if !clip.contains(x, y) {
                return false;
            }
        }

        self.break_wide_char_at(x, y);

        let idx = self.index(x, y);
        let cell_fg = fg.to_cell_color();
        let existing_bg = self.cells[idx].bg;
        let cell_bg = bg.resolve_over(&existing_bg);

        self.cells[idx] = Cell {
            ch: ch as u32,
            fg: cell_fg,
            bg: cell_bg,
            attrs,
            underline,
        };

        true
    }

    /// Fill a rectangle with a background color.
    ///
    /// All cells in the rect are reset to spaces with no attributes.
    /// If `bg` has alpha < 1.0, it's composited over existing backgrounds.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn fill_rect(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        bg: Color,
        clip: Option<&ClipRect>,
    ) {
        // Intersect the fill rect with the buffer bounds.
        let rect = ClipRect::from_unsigned(x, y, width, height);
        let Some(mut effective) = rect.intersect(self.bounds()) else {
            return;
        };

        // Further intersect with the clip rect if provided.
        if let Some(clip) = clip {
            let Some(clipped) = effective.intersect(*clip) else {
                return;
            };
            effective = clipped;
        }

        // Safe casts: intersection with bounds (origin 0,0) guarantees
        // non-negative values bounded by buffer dimensions.
        let x1 = effective.x as u16;
        let y1 = effective.y as u16;
        let x2 = effective.right() as u16;
        let y2 = effective.bottom() as u16;

        let is_opaque = bg.is_opaque();
        let opaque_bg = if is_opaque { bg.to_cell_color() } else { CellColor::Default };

        for row in y1..y2 {
            let row_start = self.index(x1, row);
            let row_end = self.index(x2, row);
            for cell in &mut self.cells[row_start..row_end] {
                cell.ch = u32::from(b' ');
                cell.fg = CellColor::Default;
                cell.attrs = Attr::empty();
                cell.underline = UnderlineStyle::None;
                cell.bg = if is_opaque {
                    opaque_bg
                } else {
                    bg.resolve_over(&cell.bg)
                };
            }
        }
    }

    // ─── Text Painting ──────────────────────────────────────────────────

    /// Paint a text string with wide-character handling and compositing.
    ///
    /// Characters are placed left-to-right starting at `(x, y)`. Wide
    /// characters (CJK, some emoji) occupy two columns; a continuation
    /// cell is placed at `x+1`. Zero-width characters are skipped.
    ///
    /// If a wide character doesn't fit at the end of the buffer, a space
    /// is painted instead (partial wide chars produce terminal garbage).
    ///
    /// Returns the number of columns consumed.
    #[allow(clippy::too_many_arguments, clippy::similar_names)]
    pub fn paint_text(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        fg: Color,
        bg: Color,
        attrs: Attr,
        underline: UnderlineStyle,
        clip: Option<&ClipRect>,
    ) -> u16 {
        if y >= self.height {
            return 0;
        }

        let mut col = x;

        for ch in text.chars() {
            if col >= self.width {
                break;
            }

            let char_w = ch.width().unwrap_or(0);
            if char_w == 0 {
                continue;
            }

            let is_wide = char_w == 2;

            // Wide chars need two columns. If the continuation doesn't fit
            // in the buffer, paint a space instead — partial wide chars are
            // display garbage in every terminal.
            if is_wide && col + 1 >= self.width {
                self.paint_cell(col, y, ' ', fg, bg, attrs, underline, clip);
                col += 1;
                break;
            }

            // Paint the main character.
            if self.paint_cell(col, y, ch, fg, bg, attrs, underline, clip) && is_wide {
                let cont_x = col + 1;
                if clip.is_none_or(|c| c.contains(cont_x, y)) {
                    // Break any wide char occupying the continuation position
                    // before we overwrite it.
                    self.break_wide_char_at(cont_x, y);

                    let cont_idx = self.index(cont_x, y);
                    let existing_bg = self.cells[cont_idx].bg;
                    let cont_bg = bg.resolve_over(&existing_bg);
                    let cont_fg = fg.to_cell_color();
                    self.cells[cont_idx] = Cell::continuation(cont_fg, cont_bg, attrs);
                }
            }

            // char_w is 1 or 2 — safe truncation to u16.
            #[allow(clippy::cast_possible_truncation)]
            let w = char_w as u16;
            col = col.saturating_add(w);
        }

        col.saturating_sub(x)
    }
}

impl std::fmt::Debug for FrameBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FrameBuffer({}x{})", self.width, self.height)
    }
}

// ─── Text Width Utilities ───────────────────────────────────────────────────────

/// Display width of a character in terminal columns.
///
/// Returns 0 for control characters, 1 for most characters, and 2 for
/// wide characters (CJK, some emoji). Uses the `unicode-width` crate
/// for accuracy per Unicode Standard Annex #11.
///
/// # Examples
///
/// ```
/// use n_term::buffer::char_width;
///
/// assert_eq!(char_width('a'), 1);
/// assert_eq!(char_width('中'), 2);
/// assert_eq!(char_width('\n'), 0);
/// ```
#[inline]
#[must_use]
pub fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// Display width of a string in terminal columns.
///
/// Sums the width of each character. Wide characters count as 2,
/// zero-width and control characters count as 0.
///
/// # Examples
///
/// ```
/// use n_term::buffer::string_width;
///
/// assert_eq!(string_width("hello"), 5);
/// assert_eq!(string_width("中文"), 4);
/// assert_eq!(string_width("a中b"), 4);
/// ```
#[must_use]
pub fn string_width(s: &str) -> usize {
    s.chars().map(|ch| ch.width().unwrap_or(0)).sum()
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Attr, Cell, UnderlineStyle};
    use crate::color::{CellColor, Color};

    // ── ClipRect ────────────────────────────────────────────────────────

    #[test]
    fn clip_rect_creation() {
        let clip = ClipRect::new(10, 20, 80, 24);
        assert_eq!(clip.x, 10);
        assert_eq!(clip.y, 20);
        assert_eq!(clip.width, 80);
        assert_eq!(clip.height, 24);
        assert_eq!(clip.right(), 90);
        assert_eq!(clip.bottom(), 44);
    }

    #[test]
    fn clip_rect_from_unsigned() {
        let clip = ClipRect::from_unsigned(5, 10, 30, 20);
        assert_eq!(clip.x, 5);
        assert_eq!(clip.y, 10);
    }

    #[test]
    fn clip_rect_contains_corners() {
        let clip = ClipRect::new(10, 10, 20, 20);
        // Inside (corners and center)
        assert!(clip.contains(10, 10)); // top-left
        assert!(clip.contains(29, 29)); // bottom-right (inclusive)
        assert!(clip.contains(20, 20)); // center
        // Outside
        assert!(!clip.contains(9, 10));  // left
        assert!(!clip.contains(30, 10)); // right (exclusive)
        assert!(!clip.contains(10, 9));  // above
        assert!(!clip.contains(10, 30)); // below (exclusive)
    }

    #[test]
    fn clip_rect_contains_with_negative_origin() {
        let clip = ClipRect::new(-5, -3, 20, 10);
        // Screen point (0, 0) is inside because -5 <= 0 < 15 and -3 <= 0 < 7
        assert!(clip.contains(0, 0));
        assert!(clip.contains(14, 6));
        assert!(!clip.contains(15, 0)); // right edge (exclusive)
    }

    #[test]
    fn clip_rect_intersect_overlap() {
        let a = ClipRect::new(0, 0, 20, 20);
        let b = ClipRect::new(10, 10, 20, 20);
        let result = a.intersect(b).unwrap();
        assert_eq!(result.x, 10);
        assert_eq!(result.y, 10);
        assert_eq!(result.width, 10);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn clip_rect_intersect_no_overlap() {
        let a = ClipRect::new(0, 0, 10, 10);
        let b = ClipRect::new(20, 20, 10, 10);
        assert!(a.intersect(b).is_none());
    }

    #[test]
    fn clip_rect_intersect_adjacent() {
        // Touching but not overlapping (exclusive right edge)
        let a = ClipRect::new(0, 0, 10, 10);
        let b = ClipRect::new(10, 0, 10, 10);
        assert!(a.intersect(b).is_none());
    }

    #[test]
    fn clip_rect_intersect_contained() {
        let outer = ClipRect::new(0, 0, 100, 100);
        let inner = ClipRect::new(10, 10, 20, 20);
        let result = outer.intersect(inner).unwrap();
        assert_eq!(result, inner);
    }

    #[test]
    fn clip_rect_is_empty() {
        assert!(ClipRect::new(0, 0, 0, 10).is_empty());
        assert!(ClipRect::new(0, 0, 10, 0).is_empty());
        assert!(ClipRect::new(0, 0, 0, 0).is_empty());
        assert!(!ClipRect::new(0, 0, 1, 1).is_empty());
    }

    // ── FrameBuffer — Construction ──────────────────────────────────────

    #[test]
    fn new_creates_correct_size() {
        let buf = FrameBuffer::new(80, 24);
        assert_eq!(buf.width(), 80);
        assert_eq!(buf.height(), 24);
        assert_eq!(buf.total_cells(), 80 * 24);
    }

    #[test]
    fn new_cells_are_empty() {
        let buf = FrameBuffer::new(10, 5);
        for (_, _, cell) in buf.iter() {
            assert!(cell.is_empty());
        }
    }

    #[test]
    fn with_bg_sets_background() {
        let bg = CellColor::Rgb(30, 30, 30);
        let buf = FrameBuffer::with_bg(10, 5, bg);
        for (_, _, cell) in buf.iter() {
            assert_eq!(cell.bg, bg);
            assert_eq!(cell.fg, CellColor::Default);
            assert_eq!(cell.ch, b' ' as u32);
        }
    }

    #[test]
    fn zero_size_buffer() {
        let buf = FrameBuffer::new(0, 0);
        assert_eq!(buf.total_cells(), 0);
        assert!(buf.get(0, 0).is_none());
    }

    #[test]
    fn zero_width_buffer() {
        let buf = FrameBuffer::new(0, 10);
        assert_eq!(buf.total_cells(), 0);
    }

    // ── FrameBuffer — Accessors ─────────────────────────────────────────

    #[test]
    fn get_in_bounds() {
        let buf = FrameBuffer::new(10, 5);
        assert!(buf.get(0, 0).is_some());
        assert!(buf.get(9, 4).is_some());
    }

    #[test]
    fn get_out_of_bounds() {
        let buf = FrameBuffer::new(10, 5);
        assert!(buf.get(10, 0).is_none());
        assert!(buf.get(0, 5).is_none());
        assert!(buf.get(10, 5).is_none());
    }

    #[test]
    fn get_mut_modifies_cell() {
        let mut buf = FrameBuffer::new(10, 5);
        if let Some(cell) = buf.get_mut(3, 2) {
            *cell = Cell::new('Z');
        }
        assert_eq!(buf.get(3, 2).unwrap().character(), Some('Z'));
    }

    #[test]
    fn bounds_matches_dimensions() {
        let buf = FrameBuffer::new(80, 24);
        let b = buf.bounds();
        assert_eq!(b.x, 0);
        assert_eq!(b.y, 0);
        assert_eq!(b.width, 80);
        assert_eq!(b.height, 24);
    }

    #[test]
    fn in_bounds_edges() {
        let buf = FrameBuffer::new(10, 5);
        assert!(buf.in_bounds(0, 0));
        assert!(buf.in_bounds(9, 4));
        assert!(!buf.in_bounds(10, 4));
        assert!(!buf.in_bounds(9, 5));
    }

    #[test]
    fn row_returns_correct_slice() {
        let mut buf = FrameBuffer::new(5, 3);
        buf.set(2, 1, Cell::new('A'));
        let row = buf.row(1).unwrap();
        assert_eq!(row.len(), 5);
        assert_eq!(row[2].character(), Some('A'));
    }

    #[test]
    fn row_out_of_bounds() {
        let buf = FrameBuffer::new(10, 5);
        assert!(buf.row(5).is_none());
    }

    #[test]
    fn row_mut_modifies() {
        let mut buf = FrameBuffer::new(5, 3);
        let row = buf.row_mut(0).unwrap();
        row[0] = Cell::new('X');
        assert_eq!(buf.get(0, 0).unwrap().character(), Some('X'));
    }

    #[test]
    fn cells_slice_length() {
        let buf = FrameBuffer::new(10, 5);
        assert_eq!(buf.cells().len(), 50);
    }

    #[test]
    fn iter_yields_correct_coordinates() {
        let buf = FrameBuffer::new(3, 2);
        let coords: Vec<(u16, u16)> = buf.iter().map(|(x, y, _)| (x, y)).collect();
        assert_eq!(
            coords,
            vec![(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1)]
        );
    }

    // ── Clear & Resize ──────────────────────────────────────────────────

    #[test]
    fn clear_resets_all_cells() {
        let mut buf = FrameBuffer::new(5, 3);
        buf.set(2, 1, Cell::new('A').with_fg(CellColor::Rgb(255, 0, 0)));
        buf.clear();
        for (_, _, cell) in buf.iter() {
            assert!(cell.is_empty());
        }
    }

    #[test]
    fn clear_with_bg_sets_background() {
        let mut buf = FrameBuffer::new(5, 3);
        buf.set(0, 0, Cell::new('X'));
        let bg = CellColor::Rgb(50, 50, 50);
        buf.clear_with_bg(bg);
        for (_, _, cell) in buf.iter() {
            assert_eq!(cell.bg, bg);
            assert_eq!(cell.ch, b' ' as u32);
        }
    }

    #[test]
    fn resize_changes_dimensions() {
        let mut buf = FrameBuffer::new(10, 5);
        buf.resize(20, 10);
        assert_eq!(buf.width(), 20);
        assert_eq!(buf.height(), 10);
        assert_eq!(buf.total_cells(), 200);
    }

    #[test]
    fn resize_clears_content() {
        let mut buf = FrameBuffer::new(10, 5);
        buf.set(0, 0, Cell::new('A'));
        buf.resize(10, 5);
        assert!(buf.get(0, 0).unwrap().is_empty());
    }

    // ── Direct Cell Access ──────────────────────────────────────────────

    #[test]
    fn set_in_bounds_succeeds() {
        let mut buf = FrameBuffer::new(10, 5);
        assert!(buf.set(5, 3, Cell::new('X')));
        assert_eq!(buf.get(5, 3).unwrap().character(), Some('X'));
    }

    #[test]
    fn set_out_of_bounds_fails() {
        let mut buf = FrameBuffer::new(10, 5);
        assert!(!buf.set(10, 0, Cell::new('X')));
        assert!(!buf.set(0, 5, Cell::new('X')));
    }

    // ── Paint — Opaque ──────────────────────────────────────────────────

    #[test]
    fn paint_cell_opaque() {
        let mut buf = FrameBuffer::new(10, 5);
        let fg = Color::srgb(1.0, 0.0, 0.0);
        let bg = Color::srgb(0.0, 0.0, 1.0);

        assert!(buf.paint_cell(3, 2, 'X', fg, bg, Attr::BOLD, UnderlineStyle::Curly, None));

        let cell = buf.get(3, 2).unwrap();
        assert_eq!(cell.character(), Some('X'));
        assert_eq!(cell.fg, fg.to_cell_color());
        assert_eq!(cell.bg, bg.to_cell_color());
        assert!(cell.attrs.contains(Attr::BOLD));
        assert_eq!(cell.underline, UnderlineStyle::Curly);
    }

    #[test]
    fn paint_cell_out_of_bounds() {
        let mut buf = FrameBuffer::new(10, 5);
        let c = Color::BLACK;
        assert!(!buf.paint_cell(10, 0, 'X', c, c, Attr::empty(), UnderlineStyle::None, None));
    }

    // ── Paint — Transparency ────────────────────────────────────────────

    #[test]
    fn paint_cell_transparent_bg_shows_existing() {
        let mut buf = FrameBuffer::with_bg(10, 5, CellColor::Rgb(0, 0, 255));
        let fg = Color::srgb(1.0, 1.0, 1.0);
        let bg = Color::TRANSPARENT;

        buf.paint_cell(3, 2, 'A', fg, bg, Attr::empty(), UnderlineStyle::None, None);

        let cell = buf.get(3, 2).unwrap();
        // Transparent bg should preserve the existing blue background
        assert_eq!(cell.bg, CellColor::Rgb(0, 0, 255));
    }

    #[test]
    fn paint_cell_semi_transparent_bg_composites() {
        let mut buf = FrameBuffer::with_bg(10, 5, CellColor::Rgb(0, 0, 255));
        let fg = Color::WHITE;
        let bg = Color::srgba(1.0, 0.0, 0.0, 0.5); // 50% red

        buf.paint_cell(3, 2, 'A', fg, bg, Attr::empty(), UnderlineStyle::None, None);

        let cell = buf.get(3, 2).unwrap();
        // Result should be a blend of red and blue (purple-ish)
        if let CellColor::Rgb(r, _, b) = cell.bg {
            assert!(r > 100, "Expected red > 100, got {r}");
            assert!(b > 100, "Expected blue > 100, got {b}");
        } else {
            panic!("Expected Rgb variant");
        }
    }

    // ── Paint — Clipping ────────────────────────────────────────────────

    #[test]
    fn paint_cell_clipped_inside() {
        let mut buf = FrameBuffer::new(20, 10);
        let clip = ClipRect::new(5, 5, 10, 5);
        let c = Color::WHITE;
        assert!(buf.paint_cell(7, 6, 'Y', c, c, Attr::empty(), UnderlineStyle::None, Some(&clip)));
        assert_eq!(buf.get(7, 6).unwrap().character(), Some('Y'));
    }

    #[test]
    fn paint_cell_clipped_outside() {
        let mut buf = FrameBuffer::new(20, 10);
        let clip = ClipRect::new(5, 5, 10, 5);
        let c = Color::WHITE;
        // (3, 6) is left of the clip rect
        assert!(!buf.paint_cell(3, 6, 'N', c, c, Attr::empty(), UnderlineStyle::None, Some(&clip)));
        assert!(buf.get(3, 6).unwrap().is_empty());
    }

    // ── Paint — Wide Character Cleanup ──────────────────────────────────

    #[test]
    fn paint_over_continuation_breaks_wide_char() {
        let mut buf = FrameBuffer::new(10, 1);
        // Place a wide char at position 3 (continuation at 4)
        let c = Color::WHITE;
        buf.paint_text(3, 0, "中", c, c, Attr::empty(), UnderlineStyle::None, None);
        assert_eq!(buf.get(3, 0).unwrap().character(), Some('中'));
        assert!(buf.get(4, 0).unwrap().is_continuation());

        // Paint a narrow char over the continuation at position 4
        buf.paint_cell(4, 0, 'x', c, c, Attr::empty(), UnderlineStyle::None, None);

        // The wide char at 3 should be broken (replaced with space)
        assert_eq!(buf.get(3, 0).unwrap().character(), Some(' '));
        assert_eq!(buf.get(4, 0).unwrap().character(), Some('x'));
    }

    #[test]
    fn paint_over_wide_char_start_cleans_continuation() {
        let mut buf = FrameBuffer::new(10, 1);
        let c = Color::WHITE;
        buf.paint_text(3, 0, "中", c, c, Attr::empty(), UnderlineStyle::None, None);

        // Paint a narrow char over the wide char start at position 3
        buf.paint_cell(3, 0, 'y', c, c, Attr::empty(), UnderlineStyle::None, None);

        // Continuation at 4 should be cleaned up
        assert_eq!(buf.get(3, 0).unwrap().character(), Some('y'));
        assert!(!buf.get(4, 0).unwrap().is_continuation());
        assert!(buf.get(4, 0).unwrap().is_empty());
    }

    // ── Fill Rect ───────────────────────────────────────────────────────

    #[test]
    fn fill_rect_opaque() {
        let mut buf = FrameBuffer::new(20, 10);
        let blue = Color::srgb(0.0, 0.0, 1.0);

        buf.fill_rect(5, 3, 10, 4, blue, None);

        // Inside the rect
        let cell = buf.get(5, 3).unwrap();
        assert_eq!(cell.bg, blue.to_cell_color());
        assert_eq!(cell.ch, b' ' as u32);

        let cell = buf.get(14, 6).unwrap();
        assert_eq!(cell.bg, blue.to_cell_color());

        // Outside the rect
        assert_eq!(buf.get(4, 3).unwrap().bg, CellColor::Default);
        assert_eq!(buf.get(15, 3).unwrap().bg, CellColor::Default);
        assert_eq!(buf.get(5, 2).unwrap().bg, CellColor::Default);
        assert_eq!(buf.get(5, 7).unwrap().bg, CellColor::Default);
    }

    #[test]
    fn fill_rect_transparent_composites() {
        let mut buf = FrameBuffer::with_bg(20, 10, CellColor::Rgb(0, 0, 255));
        let overlay = Color::srgba(1.0, 0.0, 0.0, 0.5); // 50% red

        buf.fill_rect(5, 3, 10, 4, overlay, None);

        // Inside: composited (not pure red, not pure blue)
        if let CellColor::Rgb(r, _, b) = buf.get(7, 4).unwrap().bg {
            assert!(r > 100, "Expected red > 100, got {r}");
            assert!(b > 100, "Expected blue > 100, got {b}");
        } else {
            panic!("Expected Rgb");
        }

        // Outside: unchanged blue
        assert_eq!(buf.get(4, 3).unwrap().bg, CellColor::Rgb(0, 0, 255));
    }

    #[test]
    fn fill_rect_with_clip() {
        let mut buf = FrameBuffer::new(20, 10);
        let clip = ClipRect::new(8, 0, 4, 10);
        let red = Color::srgb(1.0, 0.0, 0.0);

        buf.fill_rect(5, 3, 10, 4, red, Some(&clip));

        // Only the intersection (8..12, 3..7) should be filled
        assert_eq!(buf.get(8, 3).unwrap().bg, red.to_cell_color());
        assert_eq!(buf.get(11, 6).unwrap().bg, red.to_cell_color());
        // Outside clip
        assert_eq!(buf.get(7, 3).unwrap().bg, CellColor::Default);
        assert_eq!(buf.get(12, 3).unwrap().bg, CellColor::Default);
    }

    #[test]
    fn fill_rect_empty_does_nothing() {
        let mut buf = FrameBuffer::new(10, 5);
        let red = Color::srgb(1.0, 0.0, 0.0);
        buf.fill_rect(5, 3, 0, 4, red, None);
        // Nothing should change
        assert!(buf.get(5, 3).unwrap().is_empty());
    }

    #[test]
    fn fill_rect_clipped_to_buffer_bounds() {
        let mut buf = FrameBuffer::new(10, 5);
        let green = Color::srgb(0.0, 1.0, 0.0);

        // Rect extends beyond buffer
        buf.fill_rect(8, 3, 10, 10, green, None);

        // Inside buffer: filled
        assert_eq!(buf.get(8, 3).unwrap().bg, green.to_cell_color());
        assert_eq!(buf.get(9, 4).unwrap().bg, green.to_cell_color());
        // Outside buffer: no crash, and untouched cells remain default
        assert_eq!(buf.get(7, 3).unwrap().bg, CellColor::Default);
    }

    // ── Text Painting ───────────────────────────────────────────────────

    #[test]
    fn paint_text_ascii() {
        let mut buf = FrameBuffer::new(20, 5);
        let fg = Color::WHITE;
        let bg = Color::BLACK;

        let cols = buf.paint_text(2, 1, "Hello", fg, bg, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(cols, 5);
        assert_eq!(buf.get(2, 1).unwrap().character(), Some('H'));
        assert_eq!(buf.get(3, 1).unwrap().character(), Some('e'));
        assert_eq!(buf.get(4, 1).unwrap().character(), Some('l'));
        assert_eq!(buf.get(5, 1).unwrap().character(), Some('l'));
        assert_eq!(buf.get(6, 1).unwrap().character(), Some('o'));
    }

    #[test]
    fn paint_text_wide_chars() {
        let mut buf = FrameBuffer::new(20, 1);
        let c = Color::WHITE;

        let cols = buf.paint_text(0, 0, "中文", c, c, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(cols, 4); // Two wide chars = 4 columns
        assert_eq!(buf.get(0, 0).unwrap().character(), Some('中'));
        assert!(buf.get(1, 0).unwrap().is_continuation());
        assert_eq!(buf.get(2, 0).unwrap().character(), Some('文'));
        assert!(buf.get(3, 0).unwrap().is_continuation());
    }

    #[test]
    fn paint_text_mixed_width() {
        let mut buf = FrameBuffer::new(20, 1);
        let c = Color::WHITE;

        let cols = buf.paint_text(0, 0, "a中b", c, c, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(cols, 4); // 1 + 2 + 1
        assert_eq!(buf.get(0, 0).unwrap().character(), Some('a'));
        assert_eq!(buf.get(1, 0).unwrap().character(), Some('中'));
        assert!(buf.get(2, 0).unwrap().is_continuation());
        assert_eq!(buf.get(3, 0).unwrap().character(), Some('b'));
    }

    #[test]
    fn paint_text_wide_char_at_buffer_edge() {
        // Buffer width 5: wide char at column 4 can't fit (needs cols 4+5)
        let mut buf = FrameBuffer::new(5, 1);
        let c = Color::WHITE;

        buf.paint_text(0, 0, "abc中", c, c, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(buf.get(0, 0).unwrap().character(), Some('a'));
        assert_eq!(buf.get(1, 0).unwrap().character(), Some('b'));
        assert_eq!(buf.get(2, 0).unwrap().character(), Some('c'));
        // Wide char at col 3 would need cols 3+4 — fits!
        assert_eq!(buf.get(3, 0).unwrap().character(), Some('中'));
        assert!(buf.get(4, 0).unwrap().is_continuation());
    }

    #[test]
    fn paint_text_wide_char_doesnt_fit_at_last_column() {
        // Buffer width 4: "abc中" — 'c' at col 2, '中' at col 3 needs col 4 which doesn't exist
        let mut buf = FrameBuffer::new(4, 1);
        let c = Color::WHITE;

        buf.paint_text(0, 0, "abc中", c, c, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(buf.get(0, 0).unwrap().character(), Some('a'));
        assert_eq!(buf.get(1, 0).unwrap().character(), Some('b'));
        assert_eq!(buf.get(2, 0).unwrap().character(), Some('c'));
        // Wide char can't fit — replaced with space
        assert_eq!(buf.get(3, 0).unwrap().character(), Some(' '));
    }

    #[test]
    fn paint_text_zero_width_chars_skipped() {
        let mut buf = FrameBuffer::new(20, 1);
        let c = Color::WHITE;

        // Combining acute accent (zero-width) after 'e'
        let cols = buf.paint_text(0, 0, "e\u{0301}x", c, c, Attr::empty(), UnderlineStyle::None, None);

        // The combining accent is zero-width, so: 'e' (1) + accent (0) + 'x' (1) = 2
        assert_eq!(cols, 2);
        assert_eq!(buf.get(0, 0).unwrap().character(), Some('e'));
        assert_eq!(buf.get(1, 0).unwrap().character(), Some('x'));
    }

    #[test]
    fn paint_text_y_out_of_bounds() {
        let mut buf = FrameBuffer::new(10, 5);
        let c = Color::WHITE;
        let cols = buf.paint_text(0, 5, "test", c, c, Attr::empty(), UnderlineStyle::None, None);
        assert_eq!(cols, 0);
    }

    #[test]
    fn paint_text_with_clip() {
        let mut buf = FrameBuffer::new(20, 1);
        let clip = ClipRect::new(2, 0, 3, 1); // Only columns 2..5
        let c = Color::WHITE;

        buf.paint_text(0, 0, "ABCDE", c, c, Attr::empty(), UnderlineStyle::None, Some(&clip));

        // Columns 0,1 should be empty (clipped)
        assert!(buf.get(0, 0).unwrap().is_empty());
        assert!(buf.get(1, 0).unwrap().is_empty());
        // Columns 2,3,4 should be painted
        assert_eq!(buf.get(2, 0).unwrap().character(), Some('C'));
        assert_eq!(buf.get(3, 0).unwrap().character(), Some('D'));
        assert_eq!(buf.get(4, 0).unwrap().character(), Some('E'));
    }

    #[test]
    fn paint_text_overwrites_existing_wide_char() {
        let mut buf = FrameBuffer::new(10, 1);
        let c = Color::WHITE;

        // First: place wide char at 3 (continuation at 4)
        buf.paint_text(3, 0, "中", c, c, Attr::empty(), UnderlineStyle::None, None);

        // Then: paint narrow text starting at 3, overwriting the wide char
        buf.paint_text(3, 0, "ab", c, c, Attr::empty(), UnderlineStyle::None, None);

        assert_eq!(buf.get(3, 0).unwrap().character(), Some('a'));
        assert_eq!(buf.get(4, 0).unwrap().character(), Some('b'));
        // No orphaned continuation
        assert!(!buf.get(4, 0).unwrap().is_continuation());
    }

    #[test]
    fn paint_text_wide_char_over_existing_wide_char() {
        let mut buf = FrameBuffer::new(10, 1);
        let c = Color::WHITE;

        // Wide char 'A' at positions 3-4, wide char 'B' at positions 5-6
        buf.paint_text(3, 0, "中文", c, c, Attr::empty(), UnderlineStyle::None, None);
        assert!(buf.get(4, 0).unwrap().is_continuation());
        assert!(buf.get(6, 0).unwrap().is_continuation());

        // Paint a wide char starting at position 4 — overwrites continuation
        // of first wide char and start of second
        buf.paint_text(4, 0, "日", c, c, Attr::empty(), UnderlineStyle::None, None);

        // Position 3: first wide char broken → space
        assert_eq!(buf.get(3, 0).unwrap().character(), Some(' '));
        // Position 4-5: new wide char '日'
        assert_eq!(buf.get(4, 0).unwrap().character(), Some('日'));
        assert!(buf.get(5, 0).unwrap().is_continuation());
        // Position 6: was continuation of '文', now orphaned → cleaned up
        assert!(buf.get(6, 0).unwrap().is_empty());
    }

    // ── Text Width Utilities ────────────────────────────────────────────

    #[test]
    fn char_width_ascii() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('~'), 1);
    }

    #[test]
    fn char_width_cjk() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('日'), 2);
        assert_eq!(char_width('文'), 2);
    }

    #[test]
    fn char_width_control() {
        assert_eq!(char_width('\n'), 0);
        assert_eq!(char_width('\t'), 0);
        assert_eq!(char_width('\0'), 0);
    }

    #[test]
    fn string_width_ascii() {
        assert_eq!(string_width("hello"), 5);
        assert_eq!(string_width(""), 0);
    }

    #[test]
    fn string_width_cjk() {
        assert_eq!(string_width("中文"), 4);
    }

    #[test]
    fn string_width_mixed() {
        assert_eq!(string_width("a中b"), 4);
    }

    // ── Debug ───────────────────────────────────────────────────────────

    #[test]
    fn debug_format() {
        let buf = FrameBuffer::new(80, 24);
        assert_eq!(format!("{buf:?}"), "FrameBuffer(80x24)");
    }
}
