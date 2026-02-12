//! Theme assembly — named highlight groups for editor rendering.
//!
//! A `Theme` is a complete set of `HighlightGroup`s that the editor's view
//! layer uses to style every UI element. Colors are pre-resolved to
//! terminal-ready `CellColor` values during construction so the hot
//! rendering path never does color math.

use n_term::cell::{Attr, UnderlineStyle};
use n_term::color::{CellColor, Color};

use crate::palette::UiPalette;
use crate::pattern::PatternKind;
use crate::syntax::SyntaxPalette;

// ---------------------------------------------------------------------------
// HighlightGroup
// ---------------------------------------------------------------------------

/// A resolved style for one editor UI element.
///
/// Pre-resolved to terminal-ready values — no alpha, no OKLCH math needed
/// at render time. Just read the fields and write to cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightGroup {
    pub fg: CellColor,
    pub bg: CellColor,
    pub attrs: Attr,
    pub underline: UnderlineStyle,
}

impl HighlightGroup {
    /// Create a highlight group with just foreground color.
    #[must_use]
    pub const fn fg_only(fg: CellColor) -> Self {
        Self {
            fg,
            bg: CellColor::Default,
            attrs: Attr::empty(),
            underline: UnderlineStyle::None,
        }
    }

    /// Create a highlight group with foreground and attributes.
    #[must_use]
    pub const fn fg_attrs(fg: CellColor, attrs: Attr) -> Self {
        Self {
            fg,
            bg: CellColor::Default,
            attrs,
            underline: UnderlineStyle::None,
        }
    }

    /// Create a highlight group with foreground and background.
    #[must_use]
    pub const fn fg_bg(fg: CellColor, bg: CellColor) -> Self {
        Self {
            fg,
            bg,
            attrs: Attr::empty(),
            underline: UnderlineStyle::None,
        }
    }
}

impl Default for HighlightGroup {
    fn default() -> Self {
        Self {
            fg: CellColor::Default,
            bg: CellColor::Default,
            attrs: Attr::empty(),
            underline: UnderlineStyle::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// A complete editor theme with named highlight groups.
///
/// Every visual element in the editor has a corresponding group. The view
/// layer reads these instead of hardcoding colors.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Name of this theme (e.g., "golden-dark", "fibonacci").
    pub name: String,

    /// Whether this is a dark theme.
    pub is_dark: bool,

    // ── UI chrome groups ──────────────────────────────────────
    /// Normal text.
    pub normal: HighlightGroup,
    /// Inactive line numbers.
    pub line_nr: HighlightGroup,
    /// Active (cursor) line number.
    pub cursor_line_nr: HighlightGroup,
    /// Tilde lines past end of buffer (`~`).
    pub non_text: HighlightGroup,
    /// Active window status line.
    pub status_line: HighlightGroup,
    /// Inactive window status line.
    pub status_line_nc: HighlightGroup,
    /// Cursor line background.
    pub cursor_line: HighlightGroup,
    /// Visual selection.
    pub visual: HighlightGroup,
    /// Search matches.
    pub search: HighlightGroup,
    /// Current search match (incremental).
    pub inc_search: HighlightGroup,
    /// Window separator.
    pub vert_split: HighlightGroup,
    /// Completion popup: selected item.
    pub pmenu_sel: HighlightGroup,
    /// Completion popup: unselected items.
    pub pmenu: HighlightGroup,
    /// Error messages.
    pub error_msg: HighlightGroup,
    /// Warning messages.
    pub warning_msg: HighlightGroup,
    /// Normal messages.
    pub msg: HighlightGroup,

    // ── Color sources (for advanced consumers) ────────────────
    /// The full UI palette used to generate this theme.
    pub palette: UiPalette,
    /// The full syntax palette (ready for tree-sitter integration).
    pub syntax: SyntaxPalette,
}

/// Resolve a Color to `CellColor`, compositing semi-transparent colors over bg.
fn resolve(color: Color, bg: Color) -> CellColor {
    if color.alpha >= 1.0 {
        color.to_cell_color()
    } else {
        color.blend_over(&bg).to_cell_color()
    }
}

impl Theme {
    /// Generate a complete theme from parameters.
    ///
    /// - `pattern`: which Sacred Geometry pattern to use
    /// - `base_hue`: primary hue angle (0-360)
    /// - `is_dark`: dark theme (true) or light theme (false)
    /// - `few`: use cohesive 5-hue subset instead of full pattern
    /// - `seed`: deterministic seed for reproducible themes
    #[must_use]
    pub fn generate(
        name: &str,
        pattern: PatternKind,
        base_hue: f32,
        is_dark: bool,
        few: bool,
        seed: u32,
    ) -> Self {
        let hues = if few {
            pattern.generate_few(base_hue)
        } else {
            pattern.generate(base_hue)
        };

        let palette = UiPalette::generate(&hues, is_dark, seed);
        let syntax = SyntaxPalette::generate(
            &hues,
            palette.bg1,
            palette.bg3,
            palette.ac1.h,
            palette.ac2.h,
            is_dark,
            seed,
        );

        Self::from_palette(name, is_dark, palette, syntax)
    }

    /// Assemble a theme from pre-computed palette and syntax colors.
    fn from_palette(
        name: &str,
        is_dark: bool,
        palette: UiPalette,
        syntax: SyntaxPalette,
    ) -> Self {
        let p = &palette;
        let bg1_cc = p.bg1.to_cell_color();

        // Resolve semi-transparent UI surface colors against bg1.
        let selection_cc = resolve(p.selection, p.bg1);
        let find_match_cc = resolve(p.find_match, p.bg1);
        let line_highlight_cc = resolve(p.line_highlight, p.bg1);

        // Comment color for line numbers.
        let comment_cc = syntax.comment.to_cell_color();

        Self {
            name: name.to_string(),
            is_dark,

            normal: HighlightGroup::fg_only(p.fg1.to_cell_color()),

            line_nr: HighlightGroup::fg_only(comment_cc),

            cursor_line_nr: HighlightGroup::fg_attrs(
                p.ac1.to_cell_color(),
                Attr::empty(),
            ),

            non_text: HighlightGroup::fg_attrs(
                p.ac1.to_cell_color(),
                Attr::DIM,
            ),

            status_line: HighlightGroup {
                fg: p.fg1.to_cell_color(),
                bg: p.ac2.to_cell_color(),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            status_line_nc: HighlightGroup {
                fg: comment_cc,
                bg: p.bg2.to_cell_color(),
                attrs: Attr::empty(),
                underline: UnderlineStyle::None,
            },

            cursor_line: HighlightGroup {
                fg: CellColor::Default,
                bg: line_highlight_cc,
                attrs: Attr::empty(),
                underline: UnderlineStyle::None,
            },

            visual: HighlightGroup {
                fg: CellColor::Default,
                bg: selection_cc,
                attrs: Attr::empty(),
                underline: UnderlineStyle::None,
            },

            search: HighlightGroup {
                fg: p.fg1.to_cell_color(),
                bg: find_match_cc,
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            inc_search: HighlightGroup {
                fg: bg1_cc,
                bg: p.ac1.to_cell_color(),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            vert_split: HighlightGroup::fg_attrs(
                p.border.to_cell_color(),
                Attr::DIM,
            ),

            pmenu_sel: HighlightGroup {
                fg: bg1_cc,
                bg: p.ac1.to_cell_color(),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            pmenu: HighlightGroup::fg_bg(
                p.fg1.to_cell_color(),
                p.bg3.to_cell_color(),
            ),

            error_msg: HighlightGroup::fg_attrs(
                p.error.to_cell_color(),
                Attr::BOLD,
            ),

            warning_msg: HighlightGroup::fg_only(
                p.warning.to_cell_color(),
            ),

            msg: HighlightGroup::fg_only(p.fg1.to_cell_color()),

            palette,
            syntax,
        }
    }

    /// The default theme: Golden Ratio, hue 270 (purple), dark, seed 42.
    #[must_use]
    pub fn default_theme() -> Self {
        Self::generate("default", PatternKind::GoldenRatio, 270.0, true, false, 42)
    }

    /// Terminal-native theme — uses ANSI colors that adapt to the user's
    /// terminal palette. No RGB colors, no OKLCH math. Just the 16 standard
    /// ANSI colors and Default fg/bg. This is what a fresh editor should look
    /// like before the user opts into Sacred Geometry themes.
    #[must_use]
    pub fn terminal() -> Self {
        use CellColor::{Ansi256, Default};

        Self {
            name: "terminal".to_string(),
            is_dark: true, // Doesn't matter — we don't generate colors.

            normal: HighlightGroup::default(),

            line_nr: HighlightGroup::fg_attrs(Ansi256(8), Attr::empty()),

            cursor_line_nr: HighlightGroup::fg_attrs(Default, Attr::empty()),

            non_text: HighlightGroup::fg_attrs(Ansi256(4), Attr::DIM),

            status_line: HighlightGroup {
                fg: Default,
                bg: Default,
                attrs: Attr::BOLD.union(Attr::INVERSE),
                underline: UnderlineStyle::None,
            },

            status_line_nc: HighlightGroup {
                fg: Default,
                bg: Default,
                attrs: Attr::DIM.union(Attr::INVERSE),
                underline: UnderlineStyle::None,
            },

            cursor_line: HighlightGroup {
                fg: Default,
                bg: Default,
                attrs: Attr::empty(),
                underline: UnderlineStyle::Straight,
            },

            visual: HighlightGroup {
                fg: Default,
                bg: Default,
                attrs: Attr::INVERSE,
                underline: UnderlineStyle::None,
            },

            search: HighlightGroup {
                fg: Ansi256(0),
                bg: Ansi256(3),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            inc_search: HighlightGroup {
                fg: Ansi256(0),
                bg: Ansi256(6),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            vert_split: HighlightGroup::fg_attrs(Default, Attr::DIM),

            pmenu_sel: HighlightGroup {
                fg: Ansi256(0),
                bg: Ansi256(4),
                attrs: Attr::BOLD,
                underline: UnderlineStyle::None,
            },

            pmenu: HighlightGroup::fg_bg(Default, Ansi256(237)),

            error_msg: HighlightGroup::fg_attrs(Ansi256(1), Attr::BOLD),

            warning_msg: HighlightGroup::fg_only(Ansi256(3)),

            msg: HighlightGroup::fg_only(Default),

            palette: UiPalette::placeholder(),
            syntax: SyntaxPalette::placeholder(),
        }
    }

    /// Generate a theme using the current timestamp as seed — each call
    /// produces a unique result, perfect for interactive exploration.
    #[must_use]
    pub fn generate_random(pattern: PatternKind, base_hue: f32, is_dark: bool) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(42, |d| d.subsec_nanos());
        let name = format!("{}_{seed}", pattern.name());
        Self::generate(&name, pattern, base_hue, is_dark, false, seed)
    }

    /// Fully random theme — random pattern, random hue, time-based seed.
    #[must_use]
    pub fn generate_surprise() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(42, |d| d.subsec_nanos());
        let patterns = PatternKind::all();
        let pattern = patterns[(seed as usize) % patterns.len()];
        let hue = (seed % 360) as f32;
        let name = format!("{}_{seed}", pattern.name());
        Self::generate(&name, pattern, hue, true, false, seed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_dark() {
        let t = Theme::default_theme();
        assert!(t.is_dark);
    }

    #[test]
    fn default_theme_has_name() {
        let t = Theme::default_theme();
        assert_eq!(t.name, "default");
    }

    #[test]
    fn all_groups_have_valid_fg() {
        let t = Theme::default_theme();
        // Spot check that non-Default fg values are Rgb.
        assert!(matches!(t.status_line.fg, CellColor::Rgb(_, _, _)));
        assert!(matches!(t.error_msg.fg, CellColor::Rgb(_, _, _)));
        assert!(matches!(t.line_nr.fg, CellColor::Rgb(_, _, _)));
    }

    #[test]
    fn status_line_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.status_line.bg.is_default());
    }

    #[test]
    fn status_line_nc_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.status_line_nc.bg.is_default());
    }

    #[test]
    fn visual_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.visual.bg.is_default());
    }

    #[test]
    fn search_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.search.bg.is_default());
    }

    #[test]
    fn pmenu_sel_has_bold() {
        let t = Theme::default_theme();
        assert!(t.pmenu_sel.attrs.contains(Attr::BOLD));
    }

    #[test]
    fn error_msg_has_bold() {
        let t = Theme::default_theme();
        assert!(t.error_msg.attrs.contains(Attr::BOLD));
    }

    #[test]
    fn deterministic() {
        let a = Theme::default_theme();
        let b = Theme::default_theme();
        assert_eq!(a.normal, b.normal);
        assert_eq!(a.status_line, b.status_line);
        assert_eq!(a.search, b.search);
    }

    #[test]
    fn light_theme_generates() {
        let t = Theme::generate("light", PatternKind::GoldenRatio, 270.0, false, false, 42);
        assert!(!t.is_dark);
    }

    #[test]
    fn different_patterns_differ() {
        let a = Theme::generate("a", PatternKind::GoldenRatio, 270.0, true, false, 42);
        let b = Theme::generate("b", PatternKind::Fibonacci, 270.0, true, false, 42);
        // Very unlikely to produce identical status line bg.
        assert_ne!(a.status_line.bg, b.status_line.bg);
    }

    #[test]
    fn different_hues_differ() {
        let a = Theme::generate("a", PatternKind::GoldenRatio, 0.0, true, false, 42);
        let b = Theme::generate("b", PatternKind::GoldenRatio, 180.0, true, false, 42);
        assert_ne!(a.non_text.fg, b.non_text.fg);
    }

    #[test]
    fn few_mode_works() {
        let t = Theme::generate("few", PatternKind::GoldenRatio, 270.0, true, true, 42);
        assert_eq!(t.name, "few");
    }

    #[test]
    fn cursor_line_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.cursor_line.bg.is_default());
    }

    #[test]
    fn pmenu_has_bg() {
        let t = Theme::default_theme();
        assert!(!t.pmenu.bg.is_default());
    }

    #[test]
    fn non_text_is_dim() {
        let t = Theme::default_theme();
        assert!(t.non_text.attrs.contains(Attr::DIM));
    }

    #[test]
    fn vert_split_is_dim() {
        let t = Theme::default_theme();
        assert!(t.vert_split.attrs.contains(Attr::DIM));
    }
}
