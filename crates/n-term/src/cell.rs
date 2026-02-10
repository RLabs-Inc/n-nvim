// SPDX-License-Identifier: MIT
//
// Cell — the atomic unit of terminal rendering.
//
// Every character position on screen is a Cell. It holds a Unicode codepoint,
// foreground and background colors, text attributes, and an underline style.
// The entire rendering pipeline exists to produce, diff, and output these.
//
// Size: 16 bytes per cell, Copy-friendly, cache-line aligned with neighbors.
// A 200×50 terminal = 10,000 cells = 160 KB per FrameBuffer — trivial.
//
// Transparency model:
//
//   Colors with alpha live in the `Color` type (OKLCH space). When painting
//   to a Cell, alpha is resolved via `Color::resolve_over()` which composites
//   in linear sRGB for physical correctness. The Cell stores only the fully
//   resolved `CellColor` — no alpha. This means:
//
//   - Diff comparison is cheap (no alpha to consider)
//   - ANSI output is straightforward (no deferred compositing)
//   - Floating windows, overlays, and translucent UI elements composite
//     through the Color pipeline before writing to cells
//
// Wide characters (CJK, some emoji) occupy two columns. The first cell
// holds the codepoint; the second is a continuation cell (ch = 0). The
// renderer skips continuation cells when outputting characters but still
// applies their colors and attributes for correct background fill.

use crate::color::CellColor;

// ─── Text Attributes ─────────────────────────────────────────────────────────

bitflags::bitflags! {
    /// Text attributes stored as a compact bitfield.
    ///
    /// These map directly to SGR (Select Graphic Rendition) parameters
    /// in the ANSI escape sequence standard. Combine with bitwise OR:
    ///
    /// ```
    /// use n_term::cell::Attr;
    ///
    /// let style = Attr::BOLD | Attr::ITALIC;
    /// assert!(style.contains(Attr::BOLD));
    /// assert!(style.contains(Attr::ITALIC));
    /// assert!(!style.contains(Attr::DIM));
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Attr: u8 {
        /// SGR 1 — increased intensity.
        const BOLD          = 1 << 0;
        /// SGR 2 — decreased intensity (faint).
        const DIM           = 1 << 1;
        /// SGR 3 — italic or oblique.
        const ITALIC        = 1 << 2;
        /// SGR 5 — slow blink (< 150 per minute).
        const SLOW_BLINK    = 1 << 3;
        /// SGR 6 — rapid blink (≥ 150 per minute). Rarely supported.
        const RAPID_BLINK   = 1 << 4;
        /// SGR 7 — swap foreground and background.
        const INVERSE       = 1 << 5;
        /// SGR 8 — invisible text (not widely supported).
        const HIDDEN        = 1 << 6;
        /// SGR 9 — crossed-out text.
        const STRIKETHROUGH = 1 << 7;
    }
}

impl Attr {
    /// Whether no attributes are set.
    #[inline]
    #[must_use]
    pub const fn is_empty_flags(self) -> bool {
        self.bits() == 0
    }
}

// ─── Underline Style ─────────────────────────────────────────────────────────

/// Underline style for a cell.
///
/// Modern terminals (Kitty, `WezTerm`, Ghostty, iTerm2) support extended
/// underline styles via SGR parameters. These are essential for LSP
/// diagnostics: curly underlines for errors, dotted for hints, etc.
///
/// Having the underline style separate from [`Attr`] keeps the attribute
/// flags clean (no conflicting "is underlined" + "which style" bits) and
/// allows the renderer to emit the correct SGR sequence directly.
///
/// When `UnderlineStyle` is anything other than `None`, the cell is
/// underlined. No separate "has underline" flag needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum UnderlineStyle {
    /// No underline.
    #[default]
    None = 0,
    /// SGR 4 / 4:1 — standard straight underline.
    Straight = 1,
    /// SGR 4:2 — double underline (two straight lines).
    Double = 2,
    /// SGR 4:3 — curly/wavy underline. Used for LSP errors.
    Curly = 3,
    /// SGR 4:4 — dotted underline. Used for LSP hints.
    Dotted = 4,
    /// SGR 4:5 — dashed underline.
    Dashed = 5,
}

impl UnderlineStyle {
    /// Whether any underline is active.
    #[inline]
    #[must_use]
    pub const fn is_underlined(self) -> bool {
        !matches!(self, Self::None)
    }
}

// ─── Cell ────────────────────────────────────────────────────────────────────

/// A single terminal cell — the atom of rendering.
///
/// Every character position on the terminal screen is one `Cell`. The
/// rendering pipeline's job is to produce a grid of these, diff it against
/// the previous frame, and emit minimal ANSI escape sequences for the
/// changes.
///
/// # Layout (16 bytes)
///
/// ```text
/// ┌──────────┬──────────┬──────────┬───────┬───────────┬─────────┐
/// │ ch: u32  │ fg: Cell │ bg: Cell │ attrs │ underline │ padding │
/// │ 4 bytes  │  Color   │  Color   │  u8   │    u8     │ 2 bytes │
/// │          │ 4 bytes  │ 4 bytes  │       │           │         │
/// └──────────┴──────────┴──────────┴───────┴───────────┴─────────┘
/// ```
///
/// # Wide Characters
///
/// Characters that occupy two terminal columns (CJK, some emoji) use a
/// **continuation cell**: the first cell holds the codepoint, the second
/// has `ch = 0`. The renderer knows to skip the continuation cell's
/// character output but still applies its background color.
///
/// # Transparency
///
/// Cells store fully resolved colors — no alpha. Transparency compositing
/// happens *before* writing to a cell, through [`Color::resolve_over`]:
///
/// ```
/// use n_term::color::{Color, CellColor};
///
/// let overlay = Color::srgba(1.0, 0.0, 0.0, 0.5); // 50% red
/// let existing_bg = CellColor::Rgb(0, 0, 255);      // solid blue
/// let resolved = overlay.resolve_over(&existing_bg); // blended purple
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Unicode codepoint to display.
    ///
    /// - `0` = continuation cell (second column of a wide character)
    /// - `b' '` (32) = empty / space (the default)
    /// - Any other value = the character to render
    pub ch: u32,

    /// Foreground (text) color.
    pub fg: CellColor,

    /// Background color.
    pub bg: CellColor,

    /// Text attributes (bold, italic, dim, etc.).
    pub attrs: Attr,

    /// Underline style. `None` means no underline.
    pub underline: UnderlineStyle,
}

/// Continuation marker: a cell whose `ch` is 0 belongs to the preceding
/// wide character and should not produce character output.
const CONTINUATION: u32 = 0;

/// Default character for empty cells.
const SPACE: u32 = b' ' as u32;

impl Cell {
    /// An empty cell: space character, default colors, no attributes.
    pub const EMPTY: Self = Self {
        ch: SPACE,
        fg: CellColor::Default,
        bg: CellColor::Default,
        attrs: Attr::empty(),
        underline: UnderlineStyle::None,
    };

    /// Create a cell with a character and default styling.
    #[inline]
    #[must_use]
    pub const fn new(ch: char) -> Self {
        Self {
            ch: ch as u32,
            fg: CellColor::Default,
            bg: CellColor::Default,
            attrs: Attr::empty(),
            underline: UnderlineStyle::None,
        }
    }

    /// Create a cell with full styling.
    #[inline]
    #[must_use]
    pub const fn styled(
        ch: char,
        fg: CellColor,
        bg: CellColor,
        attrs: Attr,
        underline: UnderlineStyle,
    ) -> Self {
        Self {
            ch: ch as u32,
            fg,
            bg,
            attrs,
            underline,
        }
    }

    /// Create a continuation cell for wide characters.
    ///
    /// Continuation cells inherit the colors and attributes of their
    /// parent (the preceding wide character cell) so that the background
    /// fills correctly. The renderer skips their character output.
    #[inline]
    #[must_use]
    pub const fn continuation(fg: CellColor, bg: CellColor, attrs: Attr) -> Self {
        Self {
            ch: CONTINUATION,
            fg,
            bg,
            attrs,
            underline: UnderlineStyle::None,
        }
    }

    // ─── Queries ──────────────────────────────────────────────────────────

    /// Whether this is a continuation cell (second column of a wide char).
    #[inline]
    #[must_use]
    pub const fn is_continuation(self) -> bool {
        self.ch == CONTINUATION
    }

    /// Whether this cell is visually empty (space, default colors, no styling).
    #[inline]
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.ch == SPACE
            && self.fg == CellColor::Default
            && self.bg == CellColor::Default
            && self.attrs.is_empty_flags()
            && !self.underline.is_underlined()
    }

    /// Whether this cell has any underline active.
    #[inline]
    #[must_use]
    pub const fn is_underlined(self) -> bool {
        self.underline.is_underlined()
    }

    /// Whether this cell has any text attributes set.
    #[inline]
    #[must_use]
    pub const fn has_attrs(self) -> bool {
        !self.attrs.is_empty_flags()
    }

    /// The Unicode codepoint as a `char`, if valid.
    ///
    /// Returns `None` for continuation cells (`ch = 0`) and any
    /// invalid Unicode scalar values.
    #[inline]
    #[must_use]
    pub const fn character(self) -> Option<char> {
        if self.ch == CONTINUATION {
            return None;
        }
        char::from_u32(self.ch)
    }

    // ─── Mutations ────────────────────────────────────────────────────────

    /// Reset this cell to empty (space, default colors, no attributes).
    #[inline]
    pub const fn reset(&mut self) {
        *self = Self::EMPTY;
    }

    /// Set the foreground color.
    #[inline]
    #[must_use]
    pub const fn with_fg(self, fg: CellColor) -> Self {
        Self { fg, ..self }
    }

    /// Set the background color.
    #[inline]
    #[must_use]
    pub const fn with_bg(self, bg: CellColor) -> Self {
        Self { bg, ..self }
    }

    /// Set text attributes.
    #[inline]
    #[must_use]
    pub const fn with_attrs(self, attrs: Attr) -> Self {
        Self { attrs, ..self }
    }

    /// Set underline style.
    #[inline]
    #[must_use]
    pub const fn with_underline(self, underline: UnderlineStyle) -> Self {
        Self { underline, ..self }
    }

    /// Whether two cells have the same styling (colors, attributes, underline)
    /// regardless of character content.
    ///
    /// Useful for the renderer to decide whether it needs to emit new SGR
    /// sequences when moving between cells.
    #[inline]
    #[must_use]
    pub fn same_style(self, other: &Self) -> bool {
        self.fg == other.fg
            && self.bg == other.bg
            && self.attrs == other.attrs
            && self.underline == other.underline
    }
}

impl Default for Cell {
    #[inline]
    fn default() -> Self {
        Self::EMPTY
    }
}

impl std::fmt::Debug for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_continuation() {
            write!(f, "Cell(continuation)")
        } else {
            let ch = char::from_u32(self.ch).unwrap_or('?');
            write!(f, "Cell({ch:?}")?;
            if self.fg != CellColor::Default {
                write!(f, ", fg={:?}", self.fg)?;
            }
            if self.bg != CellColor::Default {
                write!(f, ", bg={:?}", self.bg)?;
            }
            if !self.attrs.is_empty_flags() {
                write!(f, ", {:?}", self.attrs)?;
            }
            if self.underline.is_underlined() {
                write!(f, ", {:?}", self.underline)?;
            }
            write!(f, ")")
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    // ── Layout ───────────────────────────────────────────────────────────

    #[test]
    fn cell_is_16_bytes() {
        assert_eq!(mem::size_of::<Cell>(), 16);
    }

    #[test]
    fn cell_is_copy() {
        let a = Cell::EMPTY;
        let b = a; // Copy, not move
        assert_eq!(a, b);
    }

    #[test]
    fn attr_is_1_byte() {
        assert_eq!(mem::size_of::<Attr>(), 1);
    }

    #[test]
    fn underline_style_is_1_byte() {
        assert_eq!(mem::size_of::<UnderlineStyle>(), 1);
    }

    #[test]
    fn cell_color_is_4_bytes() {
        assert_eq!(mem::size_of::<CellColor>(), 4);
    }

    // ── Default / Empty ──────────────────────────────────────────────────

    #[test]
    fn default_cell_is_empty() {
        let cell = Cell::default();
        assert!(cell.is_empty());
        assert_eq!(cell.ch, b' ' as u32);
        assert_eq!(cell.fg, CellColor::Default);
        assert_eq!(cell.bg, CellColor::Default);
        assert!(cell.attrs.is_empty_flags());
        assert!(!cell.is_underlined());
    }

    #[test]
    fn empty_constant_matches_default() {
        assert_eq!(Cell::EMPTY, Cell::default());
    }

    #[test]
    fn cell_with_non_default_fg_is_not_empty() {
        let cell = Cell::EMPTY.with_fg(CellColor::Rgb(255, 0, 0));
        assert!(!cell.is_empty());
    }

    #[test]
    fn cell_with_non_default_bg_is_not_empty() {
        let cell = Cell::EMPTY.with_bg(CellColor::Rgb(0, 0, 255));
        assert!(!cell.is_empty());
    }

    #[test]
    fn cell_with_attrs_is_not_empty() {
        let cell = Cell::EMPTY.with_attrs(Attr::BOLD);
        assert!(!cell.is_empty());
    }

    #[test]
    fn cell_with_underline_is_not_empty() {
        let cell = Cell::EMPTY.with_underline(UnderlineStyle::Curly);
        assert!(!cell.is_empty());
    }

    // ── Construction ─────────────────────────────────────────────────────

    #[test]
    fn new_cell_has_default_styling() {
        let cell = Cell::new('A');
        assert_eq!(cell.ch, u32::from('A'));
        assert_eq!(cell.fg, CellColor::Default);
        assert_eq!(cell.bg, CellColor::Default);
        assert!(cell.attrs.is_empty_flags());
        assert!(!cell.is_underlined());
    }

    #[test]
    fn styled_cell_has_all_fields() {
        let cell = Cell::styled(
            'Z',
            CellColor::Rgb(255, 255, 0),
            CellColor::Rgb(0, 0, 128),
            Attr::BOLD | Attr::ITALIC,
            UnderlineStyle::Curly,
        );
        assert_eq!(cell.ch, u32::from('Z'));
        assert_eq!(cell.fg, CellColor::Rgb(255, 255, 0));
        assert_eq!(cell.bg, CellColor::Rgb(0, 0, 128));
        assert!(cell.attrs.contains(Attr::BOLD));
        assert!(cell.attrs.contains(Attr::ITALIC));
        assert!(!cell.attrs.contains(Attr::DIM));
        assert_eq!(cell.underline, UnderlineStyle::Curly);
    }

    #[test]
    fn unicode_cell() {
        let cell = Cell::new('日');
        assert_eq!(cell.character(), Some('日'));
    }

    #[test]
    fn emoji_cell() {
        let cell = Cell::new('🔥');
        assert_eq!(cell.character(), Some('🔥'));
    }

    // ── Continuation ─────────────────────────────────────────────────────

    #[test]
    fn continuation_cell_detected() {
        let cell = Cell::continuation(CellColor::Default, CellColor::Default, Attr::empty());
        assert!(cell.is_continuation());
        assert_eq!(cell.ch, 0);
    }

    #[test]
    fn continuation_inherits_colors() {
        let fg = CellColor::Rgb(200, 100, 50);
        let bg = CellColor::Rgb(10, 20, 30);
        let cell = Cell::continuation(fg, bg, Attr::BOLD);
        assert!(cell.is_continuation());
        assert_eq!(cell.fg, fg);
        assert_eq!(cell.bg, bg);
        assert!(cell.attrs.contains(Attr::BOLD));
    }

    #[test]
    fn continuation_character_is_none() {
        let cell = Cell::continuation(CellColor::Default, CellColor::Default, Attr::empty());
        assert!(cell.character().is_none());
    }

    #[test]
    fn regular_cell_is_not_continuation() {
        let cell = Cell::new('x');
        assert!(!cell.is_continuation());
    }

    // ── Attributes ───────────────────────────────────────────────────────

    #[test]
    fn attr_combine_with_or() {
        let style = Attr::BOLD | Attr::ITALIC | Attr::STRIKETHROUGH;
        assert!(style.contains(Attr::BOLD));
        assert!(style.contains(Attr::ITALIC));
        assert!(style.contains(Attr::STRIKETHROUGH));
        assert!(!style.contains(Attr::DIM));
        assert!(!style.contains(Attr::INVERSE));
    }

    #[test]
    fn attr_all_flags_fit_in_u8() {
        let all = Attr::BOLD
            | Attr::DIM
            | Attr::ITALIC
            | Attr::SLOW_BLINK
            | Attr::RAPID_BLINK
            | Attr::INVERSE
            | Attr::HIDDEN
            | Attr::STRIKETHROUGH;
        assert_eq!(all.bits(), 0xFF);
    }

    #[test]
    fn attr_insert_and_remove() {
        let mut style = Attr::BOLD;
        style.insert(Attr::ITALIC);
        assert!(style.contains(Attr::BOLD));
        assert!(style.contains(Attr::ITALIC));

        style.remove(Attr::BOLD);
        assert!(!style.contains(Attr::BOLD));
        assert!(style.contains(Attr::ITALIC));
    }

    #[test]
    fn attr_toggle() {
        let mut style = Attr::BOLD;
        style.toggle(Attr::BOLD);
        assert!(!style.contains(Attr::BOLD));
        style.toggle(Attr::BOLD);
        assert!(style.contains(Attr::BOLD));
    }

    #[test]
    fn attr_default_is_empty() {
        let attr = Attr::default();
        assert!(attr.is_empty_flags());
        assert_eq!(attr.bits(), 0);
    }

    // ── Underline Style ──────────────────────────────────────────────────

    #[test]
    fn underline_none_is_not_underlined() {
        assert!(!UnderlineStyle::None.is_underlined());
    }

    #[test]
    fn all_underline_styles_are_underlined() {
        assert!(UnderlineStyle::Straight.is_underlined());
        assert!(UnderlineStyle::Double.is_underlined());
        assert!(UnderlineStyle::Curly.is_underlined());
        assert!(UnderlineStyle::Dotted.is_underlined());
        assert!(UnderlineStyle::Dashed.is_underlined());
    }

    #[test]
    fn underline_default_is_none() {
        assert_eq!(UnderlineStyle::default(), UnderlineStyle::None);
    }

    // ── Builder Pattern ──────────────────────────────────────────────────

    #[test]
    fn builder_chain() {
        let cell = Cell::new('A')
            .with_fg(CellColor::Rgb(255, 0, 0))
            .with_bg(CellColor::Ansi256(236))
            .with_attrs(Attr::BOLD | Attr::ITALIC)
            .with_underline(UnderlineStyle::Curly);

        assert_eq!(cell.character(), Some('A'));
        assert_eq!(cell.fg, CellColor::Rgb(255, 0, 0));
        assert_eq!(cell.bg, CellColor::Ansi256(236));
        assert!(cell.attrs.contains(Attr::BOLD));
        assert!(cell.attrs.contains(Attr::ITALIC));
        assert_eq!(cell.underline, UnderlineStyle::Curly);
    }

    // ── Reset ────────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_everything() {
        let mut cell = Cell::styled(
            'X',
            CellColor::Rgb(255, 0, 0),
            CellColor::Rgb(0, 0, 255),
            Attr::BOLD | Attr::ITALIC,
            UnderlineStyle::Double,
        );
        cell.reset();
        assert!(cell.is_empty());
        assert_eq!(cell, Cell::EMPTY);
    }

    // ── Style Comparison ─────────────────────────────────────────────────

    #[test]
    fn same_style_ignores_character() {
        let a = Cell::new('A').with_fg(CellColor::Rgb(255, 0, 0));
        let b = Cell::new('B').with_fg(CellColor::Rgb(255, 0, 0));
        assert!(a.same_style(&b));
        assert_ne!(a, b); // Different character
    }

    #[test]
    fn different_fg_is_different_style() {
        let a = Cell::new('A').with_fg(CellColor::Rgb(255, 0, 0));
        let b = Cell::new('A').with_fg(CellColor::Rgb(0, 255, 0));
        assert!(!a.same_style(&b));
    }

    #[test]
    fn different_bg_is_different_style() {
        let a = Cell::new('A').with_bg(CellColor::Rgb(255, 0, 0));
        let b = Cell::new('A').with_bg(CellColor::Rgb(0, 255, 0));
        assert!(!a.same_style(&b));
    }

    #[test]
    fn different_attrs_is_different_style() {
        let a = Cell::new('A').with_attrs(Attr::BOLD);
        let b = Cell::new('A').with_attrs(Attr::ITALIC);
        assert!(!a.same_style(&b));
    }

    #[test]
    fn different_underline_is_different_style() {
        let a = Cell::new('A').with_underline(UnderlineStyle::Straight);
        let b = Cell::new('A').with_underline(UnderlineStyle::Curly);
        assert!(!a.same_style(&b));
    }

    // ── Equality ─────────────────────────────────────────────────────────

    #[test]
    fn identical_cells_are_equal() {
        let a = Cell::styled(
            'Q',
            CellColor::Rgb(1, 2, 3),
            CellColor::Ansi256(42),
            Attr::DIM,
            UnderlineStyle::Dashed,
        );
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn cells_differ_by_character() {
        let a = Cell::new('A');
        let b = Cell::new('B');
        assert_ne!(a, b);
    }

    #[test]
    fn cells_differ_by_fg() {
        let a = Cell::EMPTY.with_fg(CellColor::Rgb(1, 2, 3));
        let b = Cell::EMPTY.with_fg(CellColor::Rgb(4, 5, 6));
        assert_ne!(a, b);
    }

    // ── Debug Format ─────────────────────────────────────────────────────

    #[test]
    fn debug_empty_cell() {
        let dbg = format!("{:?}", Cell::EMPTY);
        assert!(dbg.contains("Cell(' '"));
    }

    #[test]
    fn debug_styled_cell() {
        let cell = Cell::new('A')
            .with_fg(CellColor::Rgb(255, 0, 0))
            .with_attrs(Attr::BOLD)
            .with_underline(UnderlineStyle::Curly);
        let dbg = format!("{cell:?}");
        assert!(dbg.contains("Cell('A'"));
        assert!(dbg.contains("fg="));
        assert!(dbg.contains("BOLD"));
        assert!(dbg.contains("Curly"));
    }

    #[test]
    fn debug_continuation_cell() {
        let cell = Cell::continuation(CellColor::Default, CellColor::Default, Attr::empty());
        let dbg = format!("{cell:?}");
        assert_eq!(dbg, "Cell(continuation)");
    }

    // ── has_attrs / is_underlined ────────────────────────────────────────

    #[test]
    fn has_attrs_true_when_set() {
        let cell = Cell::EMPTY.with_attrs(Attr::BOLD);
        assert!(cell.has_attrs());
    }

    #[test]
    fn has_attrs_false_when_empty() {
        assert!(!Cell::EMPTY.has_attrs());
    }

    #[test]
    fn is_underlined_delegates_to_style() {
        let cell = Cell::EMPTY.with_underline(UnderlineStyle::Straight);
        assert!(cell.is_underlined());
    }
}
