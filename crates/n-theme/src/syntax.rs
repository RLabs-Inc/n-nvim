//! Syntax token color generation — semantic color families for code.
//!
//! Generates 30+ colors grouped by semantic role (functions, types, control
//! flow, etc.). Within each group, colors share a hue but get independently
//! randomized lightness and chroma for subtle visual distinction.
//!
//! The `SyntaxPalette` is stored in the `Theme` but not consumed by n-nvim
//! until a syntax highlighting engine (tree-sitter or LSP semantic tokens)
//! is integrated. It's generated now so themes are complete.

use n_term::color::Color;

use crate::contrast::{adjust_comment_color, ensure_readability};

// ---------------------------------------------------------------------------
// Xorshift32 (same as palette.rs, duplicated to avoid cross-module dep)
// ---------------------------------------------------------------------------

struct Xorshift32 {
    state: u32,
}

impl Xorshift32 {
    fn new(seed: u32) -> Self {
        Self { state: seed.max(1) }
    }

    const fn next(&mut self) -> u32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        self.state
    }

    fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
        let t = f64::from(self.next()) / f64::from(u32::MAX);
        (hi - lo).mul_add(t as f32, lo)
    }

    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        let idx = (self.next() as usize) % slice.len();
        &slice[idx]
    }
}

// ---------------------------------------------------------------------------
// SyntaxPalette
// ---------------------------------------------------------------------------

/// Complete set of syntax token colors for code highlighting.
///
/// Organized by semantic group. Within each group, colors share a hue
/// but vary in lightness and chroma for subtle distinction.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct SyntaxPalette {
    // ── Function group (AC1 hue) ──────────────────────────────
    pub function: Color,
    pub function_call: Color,

    // ── Method group (shared hue) ─────────────────────────────
    pub method: Color,
    pub method_call: Color,

    // ── Variable group ────────────────────────────────────────
    pub variable: Color,
    pub variable_readonly: Color,
    pub variable_declaration: Color,

    // ── Type group ────────────────────────────────────────────
    pub type_name: Color,
    pub type_parameter: Color,
    pub class: Color,

    // ── Control group ─────────────────────────────────────────
    pub control: Color,
    pub control_flow: Color,
    pub control_import: Color,

    // ── Storage ───────────────────────────────────────────────
    pub storage: Color,
    pub modifier: Color,

    // ── Keyword / operator ────────────────────────────────────
    pub keyword: Color,
    pub operator: Color,

    // ── Punctuation group ─────────────────────────────────────
    pub punctuation: Color,
    pub punctuation_bracket: Color,
    pub punctuation_delimiter: Color,

    // ── Tag group (HTML/JSX) ──────────────────────────────────
    pub tag: Color,
    pub tag_punctuation: Color,
    pub attribute: Color,

    // ── Comment ───────────────────────────────────────────────
    pub comment: Color,

    // ── Others ────────────────────────────────────────────────
    pub string: Color,
    pub constant: Color,
    pub property: Color,
    pub namespace: Color,
    pub macro_name: Color,
    pub label: Color,
}

impl SyntaxPalette {
    /// Placeholder syntax palette for ANSI-based themes.
    #[must_use]
    pub const fn placeholder() -> Self {
        let c = Color::oklch(0.7, 0.0, 0.0);
        Self {
            function: c, function_call: c, method: c, method_call: c,
            variable: c, variable_readonly: c, variable_declaration: c,
            type_name: c, type_parameter: c, class: c,
            control: c, control_flow: c, control_import: c,
            storage: c, modifier: c, keyword: c, operator: c,
            punctuation: c, punctuation_bracket: c, punctuation_delimiter: c,
            tag: c, tag_punctuation: c, attribute: c,
            comment: c, string: c, constant: c, property: c,
            namespace: c, macro_name: c, label: c,
        }
    }

    /// Generate syntax colors from a hue array and background.
    ///
    /// - `hues`: hue array from pattern (at least 1 element)
    /// - `bg1`: primary background for readability enforcement
    /// - `bg3`: gutter background for comment distinction
    /// - `ac1_hue`: base accent hue (for functions)
    /// - `ac2_hue`: secondary accent hue (for storage)
    /// - `is_dark`: dark or light theme
    /// - `seed`: deterministic RNG seed
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn generate(
        hues: &[f32],
        bg1: Color,
        bg3: Color,
        ac1_hue: f32,
        ac2_hue: f32,
        is_dark: bool,
        seed: u32,
    ) -> Self {
        let mut rng = Xorshift32::new(seed.wrapping_add(0x5678));
        let min_ratio = 5.5;

        // Pick diverse hues from the pattern FIRST (before make_color borrows rng).
        let method_hue = *rng.pick(hues);
        let var_hue = *rng.pick(hues);
        let type_hue = *rng.pick(hues);
        let control_hue = *rng.pick(hues);
        let keyword_hue = *rng.pick(hues);
        let punct_hue = *rng.pick(hues);
        let tag_hue = *rng.pick(hues);
        let string_hue = *rng.pick(hues);
        let const_hue = *rng.pick(hues);
        let prop_hue = *rng.pick(hues);
        let ns_hue = *rng.pick(hues);
        let macro_hue = *rng.pick(hues);
        let label_hue = *rng.pick(hues);

        // Comment: special treatment — de-emphasized but legible.
        let comment_chroma = rng.range_f32(0.01, 0.04);
        let comment_hue = *rng.pick(hues);
        let comment_base = Color::oklch(0.50, comment_chroma, comment_hue);
        let comment = adjust_comment_color(comment_base, bg1, bg3, is_dark);

        // Helper: make a readable color at a given hue with random variation.
        let mut make_color = |hue: f32| -> Color {
            let l = if is_dark {
                rng.range_f32(0.70, 0.85)
            } else {
                rng.range_f32(0.35, 0.55)
            };
            let c = rng.range_f32(0.08, 0.16);
            ensure_readability(Color::oklch(l, c, hue).to_gamut(), bg1, min_ratio, is_dark)
        };

        Self {
            function: make_color(ac1_hue),
            function_call: make_color(ac1_hue),
            method: make_color(method_hue),
            method_call: make_color(method_hue),
            variable: make_color(var_hue),
            variable_readonly: make_color(var_hue),
            variable_declaration: make_color(var_hue),
            type_name: make_color(type_hue),
            type_parameter: make_color(type_hue),
            class: make_color(type_hue),
            control: make_color(control_hue),
            control_flow: make_color(control_hue),
            control_import: make_color(control_hue),
            storage: make_color(ac2_hue),
            modifier: make_color(ac2_hue),
            keyword: make_color(keyword_hue),
            operator: make_color(keyword_hue),
            punctuation: make_color(punct_hue),
            punctuation_bracket: make_color(punct_hue),
            punctuation_delimiter: make_color(punct_hue),
            tag: make_color(tag_hue),
            tag_punctuation: make_color(tag_hue),
            attribute: make_color(tag_hue),
            comment,
            string: make_color(string_hue),
            constant: make_color(const_hue),
            property: make_color(prop_hue),
            namespace: make_color(ns_hue),
            macro_name: make_color(macro_hue),
            label: make_color(label_hue),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast::contrast_ratio;

    fn dark_syntax() -> SyntaxPalette {
        let bg1 = Color::oklch(0.15, 0.005, 270.0);
        let bg3 = Color::oklch(0.22, 0.008, 270.0);
        SyntaxPalette::generate(
            &[270.0, 47.5, 185.0, 322.5, 120.0],
            bg1, bg3, 270.0, 47.5, true, 42,
        )
    }

    fn light_syntax() -> SyntaxPalette {
        let bg1 = Color::oklch(0.97, 0.003, 270.0);
        let bg3 = Color::oklch(0.93, 0.005, 270.0);
        SyntaxPalette::generate(
            &[270.0, 47.5, 185.0, 322.5, 120.0],
            bg1, bg3, 270.0, 47.5, false, 42,
        )
    }

    #[test]
    fn dark_all_readable() {
        let s = dark_syntax();
        let bg = Color::oklch(0.15, 0.005, 270.0);
        let colors = [
            s.function, s.method, s.variable, s.type_name, s.control,
            s.storage, s.keyword, s.punctuation, s.tag, s.string,
            s.constant, s.property, s.namespace,
        ];
        for (i, c) in colors.iter().enumerate() {
            let ratio = contrast_ratio(*c, bg);
            assert!(ratio >= 4.5, "Color {i} dark contrast too low: {ratio}");
        }
    }

    #[test]
    fn light_all_readable() {
        let s = light_syntax();
        let bg = Color::oklch(0.97, 0.003, 270.0);
        let colors = [
            s.function, s.method, s.variable, s.type_name, s.control,
            s.storage, s.keyword, s.punctuation, s.tag, s.string,
        ];
        for (i, c) in colors.iter().enumerate() {
            let ratio = contrast_ratio(*c, bg);
            assert!(ratio >= 4.5, "Color {i} light contrast too low: {ratio}");
        }
    }

    #[test]
    fn comment_de_emphasized() {
        let s = dark_syntax();
        let bg = Color::oklch(0.15, 0.005, 270.0);
        let ratio = contrast_ratio(s.comment, bg);
        // Comments should be much less contrasty than normal text.
        assert!(ratio < 5.0, "Comment too prominent: {ratio}");
        assert!(ratio > 1.5, "Comment too invisible: {ratio}");
    }

    #[test]
    fn deterministic() {
        let a = dark_syntax();
        let b = dark_syntax();
        assert_eq!(a.function, b.function);
        assert_eq!(a.comment, b.comment);
    }

    #[test]
    fn all_in_gamut() {
        let s = dark_syntax();
        assert!(s.function.in_srgb_gamut());
        assert!(s.comment.in_srgb_gamut());
        assert!(s.string.in_srgb_gamut());
        assert!(s.type_name.in_srgb_gamut());
    }

    #[test]
    fn group_members_share_hue_family() {
        let s = dark_syntax();
        // Function group members should have similar hue.
        let diff = (s.function.h - s.function_call.h).abs();
        let diff = if diff > 180.0 { 360.0 - diff } else { diff };
        assert!(diff < 15.0, "Function group hue mismatch: {diff}");
    }
}
