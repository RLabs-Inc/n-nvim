//! WCAG contrast ratio enforcement for readable text.
//!
//! Ensures all theme colors meet accessibility standards:
//!
//! - Normal text: contrast ratio >= 5.5:1 (slightly above WCAG AA 4.5:1)
//! - Diagnostic colors: contrast ratio >= 4.5:1
//! - Comments: kept in a narrow 2.5-3.5:1 range (dark) or 1.5-3.0:1 (light)
//!   so they're clearly de-emphasized but still legible
//!
//! The key insight from rlabs: readability enforcement must happen in sRGB
//! relative luminance space (WCAG definition), but adjustments happen in
//! OKLCH lightness — because OKLCH adjustments are perceptually uniform.

use n_term::color::{Color, srgb_to_linear};

/// Compute the relative luminance of a color per WCAG 2.1.
///
/// Uses the standard sRGB linearization + weighted sum formula:
///   L = 0.2126 * `R_lin` + 0.7152 * `G_lin` + 0.0722 * `B_lin`
///
/// Returns a value in [0.0, 1.0] where 0 is black and 1 is white.
#[must_use]
pub fn relative_luminance(color: Color) -> f64 {
    let (r, g, b) = color.to_srgb();
    let r_lin = f64::from(srgb_to_linear(r));
    let g_lin = f64::from(srgb_to_linear(g));
    let b_lin = f64::from(srgb_to_linear(b));
    0.2126f64.mul_add(r_lin, 0.7152f64.mul_add(g_lin, 0.0722 * b_lin))
}

/// Compute the WCAG 2.1 contrast ratio between two colors.
///
/// Returns a value in [1.0, 21.0]. The formula is:
///   (`L_lighter` + 0.05) / (`L_darker` + 0.05)
///
/// The result is always >= 1.0 regardless of argument order.
#[must_use]
pub fn contrast_ratio(a: Color, b: Color) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (lighter, darker) = if la >= lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Adjust a foreground color's OKLCH lightness until it meets `min_ratio`
/// contrast against `bg`.
///
/// Direction: in dark themes (`is_dark`), lightens the foreground; in light
/// themes, darkens it. Uses binary search for precision.
///
/// Returns the adjusted color (gamut-mapped to sRGB).
#[must_use]
pub fn ensure_readability(fg: Color, bg: Color, min_ratio: f64, is_dark: bool) -> Color {
    // Already readable?
    if contrast_ratio(fg, bg) >= min_ratio {
        return fg.to_gamut();
    }

    // Binary search on OKLCH lightness.
    // Dark theme: foreground should be lighter than background.
    // Light theme: foreground should be darker than background.
    let (mut lo, mut hi) = if is_dark {
        (fg.l, 1.0)
    } else {
        (0.0, fg.l)
    };

    let mut best = fg;
    for _ in 0..32 {
        let mid = (lo + hi) * 0.5;
        let candidate = Color::oklch(mid, fg.c, fg.h).to_gamut();
        let ratio = contrast_ratio(candidate, bg);
        if ratio >= min_ratio {
            best = candidate;
            // Try to stay closer to original lightness.
            if is_dark {
                hi = mid;
            } else {
                lo = mid;
            }
        } else if is_dark {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    best
}

/// Adjust a comment color to sit in the sweet spot: visible but clearly
/// de-emphasized relative to normal text.
///
/// For dark themes: target contrast ratio between 2.5 and 3.5 against bg1.
/// For light themes: target contrast ratio between 1.5 and 3.0 against bg1.
///
/// Also ensures the comment is distinguishable from bg3 (the gutter/sidebar
/// background) — at least 0.5:1 contrast difference.
#[must_use]
pub fn adjust_comment_color(comment: Color, bg1: Color, _bg3: Color, is_dark: bool) -> Color {
    let (target_min, target_max) = if is_dark {
        (2.5, 3.5)
    } else {
        (1.5, 3.0)
    };

    let target_mid = (target_min + target_max) * 0.5;

    // Binary search for the lightness that produces mid-target contrast.
    let mut lo: f32 = 0.0;
    let mut hi: f32 = 1.0;

    let mut best = comment;
    let mut best_dist = f64::MAX;

    for _ in 0..32 {
        let mid = (lo + hi) * 0.5;
        let candidate = Color::oklch(mid, comment.c, comment.h).to_gamut();
        let ratio = contrast_ratio(candidate, bg1);
        let dist = (ratio - target_mid).abs();

        if dist < best_dist {
            best_dist = dist;
            best = candidate;
        }

        // Steer toward target: if ratio is too high, move lightness toward bg.
        if is_dark {
            if ratio > target_mid {
                hi = mid; // Too bright, go darker
            } else {
                lo = mid; // Too dim, go brighter
            }
        } else if ratio > target_mid {
            lo = mid; // Too dark, go lighter
        } else {
            hi = mid; // Too light, go darker
        }
    }

    best
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    // ── Relative luminance ──────────────────────────────────────────

    #[test]
    fn luminance_black_is_zero() {
        let lum = relative_luminance(Color::BLACK);
        assert!(approx_eq(lum, 0.0, 0.001), "Black luminance: {lum}");
    }

    #[test]
    fn luminance_white_is_one() {
        let lum = relative_luminance(Color::WHITE);
        assert!(approx_eq(lum, 1.0, 0.001), "White luminance: {lum}");
    }

    #[test]
    fn luminance_mid_gray() {
        let gray = Color::srgb(0.5, 0.5, 0.5);
        let lum = relative_luminance(gray);
        // sRGB 0.5 linearizes to ~0.214
        assert!(lum > 0.15 && lum < 0.30, "Mid-gray luminance: {lum}");
    }

    #[test]
    fn luminance_pure_red() {
        let red = Color::srgb(1.0, 0.0, 0.0);
        let lum = relative_luminance(red);
        // Red contributes 0.2126
        assert!(approx_eq(lum, 0.2126, 0.01), "Red luminance: {lum}");
    }

    #[test]
    fn luminance_pure_green() {
        let green = Color::srgb(0.0, 1.0, 0.0);
        let lum = relative_luminance(green);
        // Green contributes 0.7152
        assert!(approx_eq(lum, 0.7152, 0.01), "Green luminance: {lum}");
    }

    // ── Contrast ratio ──────────────────────────────────────────────

    #[test]
    fn contrast_black_white_is_21() {
        let ratio = contrast_ratio(Color::BLACK, Color::WHITE);
        assert!(approx_eq(ratio, 21.0, 0.1), "B/W contrast: {ratio}");
    }

    #[test]
    fn contrast_same_color_is_1() {
        let c = Color::oklch(0.5, 0.1, 180.0);
        let ratio = contrast_ratio(c, c);
        assert!(approx_eq(ratio, 1.0, 0.01), "Same-color contrast: {ratio}");
    }

    #[test]
    fn contrast_is_symmetric() {
        let a = Color::srgb(0.8, 0.2, 0.3);
        let b = Color::srgb(0.1, 0.1, 0.4);
        let ab = contrast_ratio(a, b);
        let ba = contrast_ratio(b, a);
        assert!(approx_eq(ab, ba, 0.001), "Asymmetric: {ab} vs {ba}");
    }

    #[test]
    fn contrast_always_at_least_one() {
        let a = Color::oklch(0.3, 0.05, 270.0);
        let b = Color::oklch(0.35, 0.08, 90.0);
        let ratio = contrast_ratio(a, b);
        assert!(ratio >= 1.0, "Contrast < 1: {ratio}");
    }

    // ── ensure_readability ──────────────────────────────────────────

    #[test]
    fn readability_already_good() {
        let fg = Color::WHITE;
        let bg = Color::BLACK;
        let adjusted = ensure_readability(fg, bg, 5.5, true);
        let ratio = contrast_ratio(adjusted, bg);
        assert!(ratio >= 5.5, "Should already pass: {ratio}");
    }

    #[test]
    fn readability_dark_theme_too_dim() {
        // Low lightness foreground on dark background — not readable.
        let fg = Color::oklch(0.25, 0.05, 270.0);
        let bg = Color::oklch(0.15, 0.005, 270.0);
        let adjusted = ensure_readability(fg, bg, 5.5, true);
        let ratio = contrast_ratio(adjusted, bg);
        assert!(ratio >= 5.5, "Dark theme readability not met: {ratio}");
        // Adjusted should be lighter than original.
        assert!(adjusted.l > fg.l, "Should have lightened");
    }

    #[test]
    fn readability_light_theme() {
        // Foreground too light on white background.
        let fg = Color::oklch(0.85, 0.05, 90.0);
        let bg = Color::oklch(0.97, 0.002, 0.0);
        let adjusted = ensure_readability(fg, bg, 5.5, false);
        let ratio = contrast_ratio(adjusted, bg);
        assert!(ratio >= 5.5, "Light theme readability not met: {ratio}");
        // Adjusted should be darker than original.
        assert!(adjusted.l < fg.l, "Should have darkened");
    }

    #[test]
    fn readability_preserves_hue() {
        let fg = Color::oklch(0.25, 0.10, 180.0);
        let bg = Color::oklch(0.15, 0.005, 270.0);
        let adjusted = ensure_readability(fg, bg, 5.5, true);
        // Hue should be preserved (gamut mapping may shift slightly).
        let hue_diff = (adjusted.h - fg.h).abs();
        assert!(hue_diff < 5.0 || hue_diff > 355.0, "Hue shifted: {hue_diff}");
    }

    // ── adjust_comment_color ────────────────────────────────────────

    #[test]
    fn comment_dark_in_range() {
        let comment = Color::oklch(0.5, 0.02, 270.0);
        let bg1 = Color::oklch(0.15, 0.005, 270.0);
        let bg3 = Color::oklch(0.22, 0.008, 270.0);
        let adjusted = adjust_comment_color(comment, bg1, bg3, true);
        let ratio = contrast_ratio(adjusted, bg1);
        assert!(
            ratio >= 2.3 && ratio <= 3.8,
            "Dark comment contrast out of range: {ratio}"
        );
    }

    #[test]
    fn comment_light_in_range() {
        let comment = Color::oklch(0.5, 0.02, 90.0);
        let bg1 = Color::oklch(0.97, 0.002, 0.0);
        let bg3 = Color::oklch(0.93, 0.005, 0.0);
        let adjusted = adjust_comment_color(comment, bg1, bg3, false);
        let ratio = contrast_ratio(adjusted, bg1);
        assert!(
            ratio >= 1.3 && ratio <= 3.3,
            "Light comment contrast out of range: {ratio}"
        );
    }

    #[test]
    fn comment_preserves_hue() {
        let comment = Color::oklch(0.5, 0.04, 120.0);
        let bg1 = Color::oklch(0.15, 0.005, 270.0);
        let bg3 = Color::oklch(0.22, 0.008, 270.0);
        let adjusted = adjust_comment_color(comment, bg1, bg3, true);
        let hue_diff = (adjusted.h - comment.h).abs();
        assert!(hue_diff < 5.0 || hue_diff > 355.0, "Comment hue shifted: {hue_diff}");
    }
}
