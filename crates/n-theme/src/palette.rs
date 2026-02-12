//! UI palette generation — the bridge from hue arrays to concrete colors.
//!
//! Takes hue arrays from [`pattern`] and generates a complete UI color palette
//! with properly constrained lightness and chroma for backgrounds, foregrounds,
//! accent colors, and diagnostics.

use n_term::color::Color;

use crate::contrast::ensure_readability;

// ---------------------------------------------------------------------------
// Xorshift32 — a minimal deterministic PRNG
// ---------------------------------------------------------------------------

/// Minimal deterministic PRNG. No external `rand` crate needed.
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

    /// Random f32 in [lo, hi].
    fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
        let t = f64::from(self.next()) / f64::from(u32::MAX);
        (hi - lo).mul_add(t as f32, lo)
    }

    /// Pick a random element from a slice.
    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        let idx = (self.next() as usize) % slice.len();
        &slice[idx]
    }
}

// ---------------------------------------------------------------------------
// UiPalette
// ---------------------------------------------------------------------------

/// The complete UI color palette for an editor theme.
///
/// All colors are in OKLCH and have been gamut-mapped to sRGB. Readability
/// constraints are enforced during construction.
#[derive(Debug, Clone)]
pub struct UiPalette {
    // ── Backgrounds ───────────────────────────────────────────
    /// Primary editor background.
    pub bg1: Color,
    /// Secondary background (sidebars, panels).
    pub bg2: Color,
    /// Tertiary background (gutter, inactive tabs).
    pub bg3: Color,

    // ── Foregrounds ───────────────────────────────────────────
    /// Primary text (code, content).
    pub fg1: Color,
    /// Secondary text (status lines, breadcrumbs).
    pub fg2: Color,
    /// Tertiary text (placeholders, disabled).
    pub fg3: Color,

    // ── Accent ────────────────────────────────────────────────
    /// Primary accent — user's chosen base hue.
    pub ac1: Color,
    /// Secondary accent — a complementary hue from the scheme.
    pub ac2: Color,

    // ── Diagnostics (fixed semantic hues) ─────────────────────
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub success: Color,

    // ── UI surfaces ───────────────────────────────────────────
    pub border: Color,
    pub selection: Color,
    pub find_match: Color,
    pub line_highlight: Color,
}

impl UiPalette {
    /// Placeholder palette for themes that don't use OKLCH generation
    /// (e.g., the terminal-default ANSI theme). These Color values are
    /// never rendered — only the `Theme`'s `HighlightGroup` fields are used.
    #[must_use]
    pub const fn placeholder() -> Self {
        let black = Color::oklch(0.0, 0.0, 0.0);
        let white = Color::oklch(1.0, 0.0, 0.0);
        let gray = Color::oklch(0.5, 0.0, 0.0);
        Self {
            bg1: black, bg2: black, bg3: black,
            fg1: white, fg2: white, fg3: gray,
            ac1: white, ac2: gray,
            error: white, warning: white, info: white, success: white,
            border: gray, selection: gray, find_match: gray, line_highlight: gray,
        }
    }

    /// Generate a UI palette from a hue array and configuration.
    ///
    /// - `hues`: the hue array from a pattern (at least 1 element)
    /// - `is_dark`: dark theme (true) or light theme (false)
    /// - `seed`: deterministic seed for subtle random variations
    #[must_use]
    pub fn generate(hues: &[f32], is_dark: bool, seed: u32) -> Self {
        let mut rng = Xorshift32::new(seed);
        let base_hue = hues[0];

        // Pick a secondary hue different from the base.
        let ac2_hue = if hues.len() > 1 {
            *rng.pick(&hues[1..])
        } else {
            (base_hue + 180.0) % 360.0
        };

        if is_dark {
            Self::generate_dark(base_hue, ac2_hue, &mut rng)
        } else {
            Self::generate_light(base_hue, ac2_hue, &mut rng)
        }
    }

    fn generate_dark(base_hue: f32, ac2_hue: f32, rng: &mut Xorshift32) -> Self {
        // Backgrounds: very low chroma, dark.
        let bg1 = Color::oklch(rng.range_f32(0.14, 0.17), rng.range_f32(0.002, 0.008), base_hue).to_gamut();
        let bg2 = Color::oklch(bg1.l + rng.range_f32(0.02, 0.04), rng.range_f32(0.003, 0.010), base_hue).to_gamut();
        let bg3 = Color::oklch(bg2.l + rng.range_f32(0.02, 0.04), rng.range_f32(0.004, 0.012), base_hue).to_gamut();

        // Foregrounds: near-achromatic, bright.
        let fg1 = Color::oklch(rng.range_f32(0.90, 0.97), rng.range_f32(0.000, 0.010), base_hue).to_gamut();
        let fg2 = Color::oklch(rng.range_f32(0.75, 0.85), rng.range_f32(0.000, 0.008), base_hue).to_gamut();
        let fg3 = Color::oklch(rng.range_f32(0.55, 0.65), rng.range_f32(0.000, 0.008), base_hue).to_gamut();

        // Accent colors: moderate chroma.
        let ac1 = ensure_readability(
            Color::oklch(rng.range_f32(0.70, 0.80), rng.range_f32(0.10, 0.16), base_hue).to_gamut(),
            bg1, 4.5, true,
        );
        let ac2 = ensure_readability(
            Color::oklch(rng.range_f32(0.70, 0.80), rng.range_f32(0.10, 0.16), ac2_hue).to_gamut(),
            bg1, 4.5, true,
        );

        // Diagnostics: fixed semantic hues.
        let error = ensure_readability(
            Color::oklch(0.70, 0.18, rng.range_f32(24.0, 32.0)).to_gamut(),
            bg1, 4.5, true,
        );
        let warning = ensure_readability(
            Color::oklch(0.78, 0.14, rng.range_f32(70.0, 85.0)).to_gamut(),
            bg1, 4.5, true,
        );
        let info = ensure_readability(
            Color::oklch(0.72, 0.12, rng.range_f32(240.0, 270.0)).to_gamut(),
            bg1, 4.5, true,
        );
        let success = ensure_readability(
            Color::oklch(0.72, 0.14, rng.range_f32(140.0, 155.0)).to_gamut(),
            bg1, 4.5, true,
        );

        // UI surfaces.
        let border = Color::oklch(rng.range_f32(0.30, 0.38), rng.range_f32(0.005, 0.02), base_hue).to_gamut();
        let selection = Color::oklcha(rng.range_f32(0.40, 0.50), rng.range_f32(0.04, 0.08), base_hue, 0.35).to_gamut();
        let find_match = Color::oklcha(rng.range_f32(0.70, 0.80), rng.range_f32(0.12, 0.18), 85.0, 0.45).to_gamut();
        let line_highlight = Color::oklcha(bg1.l + 0.04, rng.range_f32(0.002, 0.008), base_hue, 0.5).to_gamut();

        Self {
            bg1, bg2, bg3, fg1, fg2, fg3, ac1, ac2,
            error, warning, info, success,
            border, selection, find_match, line_highlight,
        }
    }

    fn generate_light(base_hue: f32, ac2_hue: f32, rng: &mut Xorshift32) -> Self {
        // Backgrounds: very low chroma, light.
        let bg1 = Color::oklch(rng.range_f32(0.96, 0.98), rng.range_f32(0.002, 0.006), base_hue).to_gamut();
        let bg2 = Color::oklch(bg1.l - rng.range_f32(0.02, 0.04), rng.range_f32(0.003, 0.010), base_hue).to_gamut();
        let bg3 = Color::oklch(bg2.l - rng.range_f32(0.02, 0.04), rng.range_f32(0.004, 0.012), base_hue).to_gamut();

        // Foregrounds: near-achromatic, dark.
        let fg1 = Color::oklch(rng.range_f32(0.10, 0.18), rng.range_f32(0.000, 0.010), base_hue).to_gamut();
        let fg2 = Color::oklch(rng.range_f32(0.25, 0.35), rng.range_f32(0.000, 0.008), base_hue).to_gamut();
        let fg3 = Color::oklch(rng.range_f32(0.45, 0.55), rng.range_f32(0.000, 0.008), base_hue).to_gamut();

        // Accent colors.
        let ac1 = ensure_readability(
            Color::oklch(rng.range_f32(0.45, 0.55), rng.range_f32(0.12, 0.18), base_hue).to_gamut(),
            bg1, 4.5, false,
        );
        let ac2 = ensure_readability(
            Color::oklch(rng.range_f32(0.45, 0.55), rng.range_f32(0.12, 0.18), ac2_hue).to_gamut(),
            bg1, 4.5, false,
        );

        // Diagnostics.
        let error = ensure_readability(
            Color::oklch(0.55, 0.18, rng.range_f32(24.0, 32.0)).to_gamut(),
            bg1, 4.5, false,
        );
        let warning = ensure_readability(
            Color::oklch(0.50, 0.14, rng.range_f32(70.0, 85.0)).to_gamut(),
            bg1, 4.5, false,
        );
        let info = ensure_readability(
            Color::oklch(0.50, 0.12, rng.range_f32(240.0, 270.0)).to_gamut(),
            bg1, 4.5, false,
        );
        let success = ensure_readability(
            Color::oklch(0.50, 0.14, rng.range_f32(140.0, 155.0)).to_gamut(),
            bg1, 4.5, false,
        );

        // UI surfaces.
        let border = Color::oklch(rng.range_f32(0.75, 0.82), rng.range_f32(0.005, 0.02), base_hue).to_gamut();
        let selection = Color::oklcha(rng.range_f32(0.60, 0.70), rng.range_f32(0.06, 0.10), base_hue, 0.25).to_gamut();
        let find_match = Color::oklcha(rng.range_f32(0.80, 0.88), rng.range_f32(0.12, 0.18), 85.0, 0.40).to_gamut();
        let line_highlight = Color::oklcha(bg1.l - 0.03, rng.range_f32(0.002, 0.006), base_hue, 0.5).to_gamut();

        Self {
            bg1, bg2, bg3, fg1, fg2, fg3, ac1, ac2,
            error, warning, info, success,
            border, selection, find_match, line_highlight,
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

    fn dark_palette() -> UiPalette {
        UiPalette::generate(&[270.0, 47.5, 185.0, 322.5], true, 42)
    }

    fn light_palette() -> UiPalette {
        UiPalette::generate(&[270.0, 47.5, 185.0, 322.5], false, 42)
    }

    #[test]
    fn dark_bg_is_dark() {
        let p = dark_palette();
        assert!(p.bg1.l < 0.25, "bg1 too light: {}", p.bg1.l);
        assert!(p.bg2.l < 0.30, "bg2 too light: {}", p.bg2.l);
        assert!(p.bg3.l < 0.35, "bg3 too light: {}", p.bg3.l);
    }

    #[test]
    fn dark_fg_is_bright() {
        let p = dark_palette();
        assert!(p.fg1.l > 0.80, "fg1 too dark: {}", p.fg1.l);
    }

    #[test]
    fn dark_fg1_readable() {
        let p = dark_palette();
        let ratio = contrast_ratio(p.fg1, p.bg1);
        assert!(ratio >= 5.0, "fg1/bg1 contrast too low: {ratio}");
    }

    #[test]
    fn dark_ac1_readable() {
        let p = dark_palette();
        let ratio = contrast_ratio(p.ac1, p.bg1);
        assert!(ratio >= 4.0, "ac1/bg1 contrast too low: {ratio}");
    }

    #[test]
    fn dark_error_readable() {
        let p = dark_palette();
        let ratio = contrast_ratio(p.error, p.bg1);
        assert!(ratio >= 4.0, "error/bg1 contrast too low: {ratio}");
    }

    #[test]
    fn light_bg_is_light() {
        let p = light_palette();
        assert!(p.bg1.l > 0.90, "bg1 too dark: {}", p.bg1.l);
    }

    #[test]
    fn light_fg_is_dark() {
        let p = light_palette();
        assert!(p.fg1.l < 0.25, "fg1 too light: {}", p.fg1.l);
    }

    #[test]
    fn light_fg1_readable() {
        let p = light_palette();
        let ratio = contrast_ratio(p.fg1, p.bg1);
        assert!(ratio >= 5.0, "fg1/bg1 contrast too low: {ratio}");
    }

    #[test]
    fn deterministic() {
        let a = UiPalette::generate(&[270.0, 47.5], true, 42);
        let b = UiPalette::generate(&[270.0, 47.5], true, 42);
        assert_eq!(a.bg1, b.bg1);
        assert_eq!(a.fg1, b.fg1);
        assert_eq!(a.ac1, b.ac1);
    }

    #[test]
    fn different_seeds_differ() {
        let a = UiPalette::generate(&[270.0], true, 42);
        let b = UiPalette::generate(&[270.0], true, 99);
        // Very unlikely to produce identical bg1 with different seeds.
        assert_ne!(a.bg1.l, b.bg1.l);
    }

    #[test]
    fn single_hue_works() {
        let p = UiPalette::generate(&[180.0], true, 42);
        assert!(p.bg1.l < 0.25);
        assert!(p.fg1.l > 0.80);
    }

    #[test]
    fn bg_ordering_dark() {
        let p = dark_palette();
        assert!(p.bg1.l <= p.bg2.l, "bg1 should be darkest");
        assert!(p.bg2.l <= p.bg3.l, "bg2 should be between bg1 and bg3");
    }

    #[test]
    fn bg_ordering_light() {
        let p = light_palette();
        assert!(p.bg1.l >= p.bg2.l, "bg1 should be lightest");
        assert!(p.bg2.l >= p.bg3.l, "bg2 should be between bg1 and bg3");
    }

    #[test]
    fn bg_low_chroma() {
        let p = dark_palette();
        assert!(p.bg1.c < 0.02, "bg1 chroma too high: {}", p.bg1.c);
        assert!(p.bg2.c < 0.02, "bg2 chroma too high: {}", p.bg2.c);
    }

    #[test]
    fn all_colors_in_gamut() {
        let p = dark_palette();
        assert!(p.bg1.in_srgb_gamut(), "bg1 out of gamut");
        assert!(p.fg1.in_srgb_gamut(), "fg1 out of gamut");
        assert!(p.ac1.in_srgb_gamut(), "ac1 out of gamut");
        assert!(p.error.in_srgb_gamut(), "error out of gamut");
    }

    #[test]
    fn diagnostics_distinct_hues() {
        let p = dark_palette();
        // Error (red ~25) and success (green ~145) should be far apart.
        let diff = (p.error.h - p.success.h).abs();
        assert!(diff > 60.0, "Error/success hues too close: {diff}");
    }
}
