// SPDX-License-Identifier: MIT
//
// n-term color system — OKLCH-native with full perceptual color support.
//
// Single-character variable names (r, g, b, l, c, h, a, s, m) are the
// standard mathematical convention in color science. Renaming them would
// make the code harder to compare against reference implementations.
#![allow(clippy::many_single_char_names)]
//
// The terminal world has been stuck in raw RGB for decades. This module
// brings perceptually uniform color to terminal rendering, with OKLCH
// at its core. Every color operation (lighten, darken, shift hue, blend)
// happens in perceptual space, producing results that look correct to
// human eyes — not just mathematically convenient.
//
// Conversion pipeline:
//
//   OKLCH ↔ Oklab ↔ Linear sRGB ↔ sRGB ↔ Terminal output (ANSI/256/TrueColor)
//
// Alpha blending happens in linear sRGB space for physical correctness.
// Gamut mapping clamps to sRGB when OKLCH values fall outside the displayable range.

use std::fmt;

// ─── Color ───────────────────────────────────────────────────────────────────

/// A perceptual color stored in OKLCH space with alpha transparency.
///
/// OKLCH is a cylindrical representation of the Oklab color space, designed
/// by Björn Ottosson. It provides perceptually uniform lightness, chroma,
/// and hue — meaning equal numerical steps produce equal visual steps.
///
/// This makes it ideal for:
/// - Generating harmonious color palettes (shift hue by equal angles)
/// - Adjusting brightness without hue shifts
/// - Creating smooth gradients that look uniform
/// - Mathematical theme generation (Sacred Geometry patterns)
///
/// # Examples
///
/// ```
/// use n_term::color::Color;
///
/// // Create from OKLCH values directly
/// let warm_red = Color::oklch(0.63, 0.26, 29.2);
///
/// // Create from familiar sRGB
/// let blue = Color::srgb(0.0, 0.0, 1.0);
///
/// // Create from hex
/// let green = Color::hex("#00ff00").unwrap();
///
/// // Perceptual operations
/// let lighter = warm_red.lighten(0.1);
/// let complement = warm_red.shift_hue(180.0);
/// let muted = warm_red.desaturate(0.5);
///
/// // Alpha blending
/// let overlay = warm_red.with_alpha(0.5).blend_over(&blue);
/// ```
#[derive(Clone, Copy)]
pub struct Color {
    /// Lightness: 0.0 (black) to 1.0 (white).
    pub l: f32,

    /// Chroma (colorfulness): 0.0 (gray) to ~0.37 (most vivid).
    /// Unbounded in theory, but sRGB gamut limits practical values.
    pub c: f32,

    /// Hue angle in degrees: 0.0 to 360.0.
    /// 0° = pink/red, 90° = yellow, 180° = cyan/green, 270° = blue/purple.
    pub h: f32,

    /// Alpha (opacity): 0.0 (fully transparent) to 1.0 (fully opaque).
    pub alpha: f32,
}

impl Color {
    // ─── Constructors ────────────────────────────────────────────────────

    /// Create a color from OKLCH values.
    ///
    /// - `l`: Lightness, 0.0 to 1.0
    /// - `c`: Chroma, 0.0 to ~0.37
    /// - `h`: Hue angle in degrees, 0.0 to 360.0
    #[inline]
    #[must_use]
    pub const fn oklch(l: f32, c: f32, h: f32) -> Self {
        Self { l, c, h, alpha: 1.0 }
    }

    /// Create a color from OKLCH values with alpha.
    #[inline]
    #[must_use]
    pub const fn oklcha(l: f32, c: f32, h: f32, alpha: f32) -> Self {
        Self { l, c, h, alpha }
    }

    /// Create a color from sRGB values (0.0 to 1.0 range).
    #[must_use]
    pub fn srgb(r: f32, g: f32, b: f32) -> Self {
        let (l, c, h) = srgb_to_oklch(r, g, b);
        Self { l, c, h, alpha: 1.0 }
    }

    /// Create a color from sRGB values with alpha.
    #[must_use]
    pub fn srgba(r: f32, g: f32, b: f32, alpha: f32) -> Self {
        let (l, c, h) = srgb_to_oklch(r, g, b);
        Self { l, c, h, alpha }
    }

    /// Create a color from 8-bit sRGB values (0 to 255).
    #[must_use]
    pub fn rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::srgb(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        )
    }

    /// Create a color from 8-bit sRGB values with alpha.
    #[must_use]
    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::srgba(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            f32::from(a) / 255.0,
        )
    }

    /// Create a color from a hex string.
    ///
    /// Supports: `#RGB`, `#RGBA`, `#RRGGBB`, `#RRGGBBAA` (with or without `#`).
    ///
    /// # Errors
    ///
    /// Returns `None` if the string is not a valid hex color.
    #[must_use]
    pub fn hex(s: &str) -> Option<Self> {
        parse_hex(s)
    }

    /// Create a pure gray color at the given lightness.
    ///
    /// Uses OKLCH lightness, so 0.5 is perceptual mid-gray (not sRGB 128).
    #[inline]
    #[must_use]
    pub const fn gray(lightness: f32) -> Self {
        Self::oklch(lightness, 0.0, 0.0)
    }

    /// Pure black.
    pub const BLACK: Self = Self::oklch(0.0, 0.0, 0.0);

    /// Pure white.
    pub const WHITE: Self = Self::oklch(1.0, 0.0, 0.0);

    /// Fully transparent (invisible).
    pub const TRANSPARENT: Self = Self::oklcha(0.0, 0.0, 0.0, 0.0);

    // ─── Alpha ───────────────────────────────────────────────────────────

    /// Return a copy with the given alpha value.
    #[inline]
    #[must_use]
    pub const fn with_alpha(self, alpha: f32) -> Self {
        Self { alpha, ..self }
    }

    /// Whether this color is fully opaque (alpha >= 1.0).
    #[inline]
    #[must_use]
    pub fn is_opaque(self) -> bool {
        self.alpha >= 1.0
    }

    /// Whether this color is fully transparent (alpha <= 0.0).
    #[inline]
    #[must_use]
    pub fn is_transparent(self) -> bool {
        self.alpha <= 0.0
    }

    /// Whether this color is achromatic (no visible chroma).
    #[inline]
    #[must_use]
    pub fn is_achromatic(self) -> bool {
        self.c.abs() < 1e-5
    }

    // ─── Perceptual Operations ───────────────────────────────────────────
    //
    // These all work in OKLCH space, producing perceptually uniform results.

    /// Increase lightness by `amount` (clamped to 0.0–1.0).
    #[inline]
    #[must_use]
    pub fn lighten(self, amount: f32) -> Self {
        Self {
            l: (self.l + amount).clamp(0.0, 1.0),
            ..self
        }
    }

    /// Decrease lightness by `amount` (clamped to 0.0–1.0).
    #[inline]
    #[must_use]
    pub fn darken(self, amount: f32) -> Self {
        Self {
            l: (self.l - amount).clamp(0.0, 1.0),
            ..self
        }
    }

    /// Set lightness to an absolute value (clamped to 0.0–1.0).
    #[inline]
    #[must_use]
    pub const fn set_lightness(self, l: f32) -> Self {
        Self {
            l: l.clamp(0.0, 1.0),
            ..self
        }
    }

    /// Increase chroma (color intensity) by `amount`.
    #[inline]
    #[must_use]
    pub fn saturate(self, amount: f32) -> Self {
        Self {
            c: (self.c + amount).max(0.0),
            ..self
        }
    }

    /// Decrease chroma by `amount` (clamped to 0.0).
    #[inline]
    #[must_use]
    pub fn desaturate(self, amount: f32) -> Self {
        Self {
            c: (self.c - amount).max(0.0),
            ..self
        }
    }

    /// Set chroma to an absolute value (clamped to >= 0.0).
    #[inline]
    #[must_use]
    pub const fn set_chroma(self, c: f32) -> Self {
        Self {
            c: c.max(0.0),
            ..self
        }
    }

    /// Shift the hue by `degrees` (wraps around 360°).
    #[inline]
    #[must_use]
    pub fn shift_hue(self, degrees: f32) -> Self {
        Self {
            h: normalize_hue(self.h + degrees),
            ..self
        }
    }

    /// Set hue to an absolute angle (normalized to 0°–360°).
    #[inline]
    #[must_use]
    pub fn set_hue(self, h: f32) -> Self {
        Self {
            h: normalize_hue(h),
            ..self
        }
    }

    /// Get the complementary color (hue shifted 180°).
    #[inline]
    #[must_use]
    pub fn complement(self) -> Self {
        self.shift_hue(180.0)
    }

    /// Mix this color with another in OKLCH space.
    ///
    /// `t` = 0.0 returns `self`, `t` = 1.0 returns `other`.
    /// Hue interpolation takes the shortest path around the color wheel.
    #[must_use]
    pub fn mix(self, other: &Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let inv_t = 1.0 - t;

        // Shortest-path hue interpolation
        let h = if self.is_achromatic() {
            other.h
        } else if other.is_achromatic() {
            self.h
        } else {
            interpolate_hue(self.h, other.h, t)
        };

        Self {
            l: self.l.mul_add(inv_t, other.l * t),
            c: self.c.mul_add(inv_t, other.c * t),
            h,
            alpha: self.alpha.mul_add(inv_t, other.alpha * t),
        }
    }

    /// Compute the perceptual distance to another color.
    ///
    /// Uses Delta E in Oklab space (Euclidean distance in L, a, b).
    /// This is a simple but effective perceptual distance metric.
    /// Values below ~0.02 are generally imperceptible.
    #[must_use]
    pub fn distance(self, other: &Self) -> f32 {
        let (l1, a1, b1) = oklch_to_oklab(self.l, self.c, self.h);
        let (l2, a2, b2) = oklch_to_oklab(other.l, other.c, other.h);
        let dl = l1 - l2;
        let da = a1 - a2;
        let db = b1 - b2;
        db.mul_add(db, dl.mul_add(dl, da * da)).sqrt()
    }

    // ─── Alpha Blending ──────────────────────────────────────────────────

    /// Composite this color (source) over another (destination).
    ///
    /// Uses Porter-Duff "source over" in linear sRGB for physical correctness.
    /// This is the standard compositing operation used in all graphics.
    #[must_use]
    pub fn blend_over(self, dst: &Self) -> Self {
        // Fast paths
        if self.is_opaque() || dst.is_transparent() {
            return self;
        }
        if self.is_transparent() {
            return *dst;
        }

        // Convert both to linear sRGB for physically correct blending
        let (sr, sg, sb) = self.to_linear_srgb();
        let sa = self.alpha;
        let (dr, dg, db) = dst.to_linear_srgb();
        let da = dst.alpha;

        // Porter-Duff "source over"
        let out_a = da.mul_add(1.0 - sa, sa);
        if out_a < 1e-6 {
            return Self::TRANSPARENT;
        }

        let inv_sa = 1.0 - sa;
        let out_r = sr.mul_add(sa, dr * da * inv_sa) / out_a;
        let out_g = sg.mul_add(sa, dg * da * inv_sa) / out_a;
        let out_b = sb.mul_add(sa, db * da * inv_sa) / out_a;

        // Convert back through sRGB → OKLCH
        let r = linear_to_srgb(out_r);
        let g = linear_to_srgb(out_g);
        let b = linear_to_srgb(out_b);
        let (l, c, h) = srgb_to_oklch(r, g, b);

        Self { l, c, h, alpha: out_a }
    }

    // ─── Conversions to sRGB ─────────────────────────────────────────────

    /// Convert to sRGB with gamut mapping (values clamped to 0.0–1.0).
    #[must_use]
    pub fn to_srgb(self) -> (f32, f32, f32) {
        let (r, g, b) = oklch_to_srgb(self.l, self.c, self.h);
        (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
    }

    /// Convert to 8-bit sRGB with gamut mapping.
    #[must_use]
    pub fn to_rgb8(self) -> (u8, u8, u8) {
        let (r, g, b) = self.to_srgb();
        (to_u8(r), to_u8(g), to_u8(b))
    }

    /// Convert to 8-bit sRGBA with gamut mapping.
    #[must_use]
    pub fn to_rgba8(self) -> (u8, u8, u8, u8) {
        let (r, g, b) = self.to_rgb8();
        (r, g, b, to_u8(self.alpha.clamp(0.0, 1.0)))
    }

    /// Convert to linear sRGB (for blending operations).
    #[must_use]
    fn to_linear_srgb(self) -> (f32, f32, f32) {
        let (a, b) = oklch_to_oklab_ab(self.c, self.h);
        let (r, g, bl) = oklab_to_linear_srgb(self.l, a, b);
        (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), bl.clamp(0.0, 1.0))
    }

    /// Convert to hex string (`#RRGGBB` or `#RRGGBBAA` if alpha < 1.0).
    #[must_use]
    pub fn to_hex(self) -> String {
        let (r, g, b) = self.to_rgb8();
        if self.is_opaque() {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            let a = to_u8(self.alpha.clamp(0.0, 1.0));
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }

    /// Whether this color is within the sRGB gamut.
    ///
    /// Colors outside the gamut will be clamped during conversion,
    /// which can shift the perceived hue. Use this to check before
    /// converting, and consider reducing chroma to bring it in-gamut.
    #[must_use]
    pub fn in_srgb_gamut(self) -> bool {
        let (r, g, b) = oklch_to_srgb(self.l, self.c, self.h);
        (0.0..=1.0).contains(&r) && (0.0..=1.0).contains(&g) && (0.0..=1.0).contains(&b)
    }

    /// Reduce chroma until this color fits within the sRGB gamut.
    ///
    /// Uses binary search to find the maximum chroma that stays in-gamut,
    /// preserving the hue and lightness as closely as possible.
    #[must_use]
    pub fn to_gamut(self) -> Self {
        if self.in_srgb_gamut() {
            return self;
        }

        // Binary search for maximum in-gamut chroma
        let mut lo: f32 = 0.0;
        let mut hi: f32 = self.c;

        for _ in 0..16 {
            let mid = (lo + hi) * 0.5;
            let candidate = Self { c: mid, ..self };
            if candidate.in_srgb_gamut() {
                lo = mid;
            } else {
                hi = mid;
            }
        }

        Self { c: lo, ..self }
    }

    // ─── Conversion to Terminal Output ───────────────────────────────────

    /// Convert to a [`CellColor`] for terminal rendering.
    ///
    /// Produces a `TrueColor` value (24-bit RGB). For terminals that don't
    /// support `TrueColor`, use [`CellColor::to_ansi256`] or
    /// [`CellColor::to_ansi16`] for fallback.
    ///
    /// Alpha is discarded — this produces the raw color without compositing.
    /// For colors with alpha, use [`resolve_over`](Self::resolve_over) to
    /// composite against a background first.
    #[must_use]
    pub fn to_cell_color(self) -> CellColor {
        let (r, g, b) = self.to_rgb8();
        CellColor::Rgb(r, g, b)
    }

    /// Resolve this color to a terminal-ready [`CellColor`], compositing
    /// over the given background if this color has alpha < 1.0.
    ///
    /// This is the bridge between the rich `Color` type (OKLCH with alpha)
    /// and the compact `CellColor` (terminal output, no alpha). Use this
    /// when painting semi-transparent overlays onto existing cells:
    ///
    /// ```
    /// use n_term::color::{Color, CellColor};
    ///
    /// // A 50% transparent red overlay on a blue background
    /// let overlay = Color::srgba(1.0, 0.0, 0.0, 0.5);
    /// let existing = CellColor::Rgb(0, 0, 255);
    /// let resolved = overlay.resolve_over(&existing);
    ///
    /// // Result is a blended purple — fully resolved, ready for terminal output
    /// assert!(matches!(resolved, CellColor::Rgb(_, _, _)));
    /// ```
    ///
    /// Compositing happens in linear sRGB (physically correct, no dark seams).
    #[must_use]
    pub fn resolve_over(self, background: &CellColor) -> CellColor {
        // Fully opaque — no blending needed
        if self.is_opaque() {
            return self.to_cell_color();
        }

        // Fully transparent — background shows through
        if self.is_transparent() {
            return *background;
        }

        // Semi-transparent — composite over background
        let bg_color = background.to_color().unwrap_or(Self::BLACK);
        self.blend_over(&bg_color).to_cell_color()
    }

    /// Resolve this color to a terminal-ready [`CellColor`], compositing
    /// over black if this color has alpha < 1.0.
    ///
    /// Convenience method for the common case where the background is
    /// unknown or black (e.g., cleared screen, first paint).
    #[must_use]
    pub fn resolve(self) -> CellColor {
        if self.is_opaque() {
            self.to_cell_color()
        } else {
            self.blend_over(&Self::BLACK).to_cell_color()
        }
    }

    /// Find the nearest ANSI-256 palette color using perceptual distance.
    ///
    /// This uses Oklab Delta E (perceptual distance) instead of naive
    /// Euclidean RGB distance, producing more visually accurate matches.
    #[must_use]
    pub fn nearest_ansi256(self) -> u8 {
        ansi::nearest_ansi256(self)
    }

    /// Find the nearest ANSI-16 color using perceptual distance.
    #[must_use]
    pub fn nearest_ansi16(self) -> u8 {
        ansi::nearest_ansi16(self)
    }
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_opaque() {
            write!(f, "Color::oklch({:.4}, {:.4}, {:.1})", self.l, self.c, self.h)
        } else {
            write!(
                f,
                "Color::oklcha({:.4}, {:.4}, {:.1}, {:.2})",
                self.l, self.c, self.h, self.alpha
            )
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl PartialEq for Color {
    fn eq(&self, other: &Self) -> bool {
        // Compare with small epsilon for floating point
        const EPS: f32 = 1e-5;
        (self.l - other.l).abs() < EPS
            && (self.c - other.c).abs() < EPS
            && (self.alpha - other.alpha).abs() < EPS
            && (self.is_achromatic()
                || other.is_achromatic()
                || hue_diff(self.h, other.h) < EPS)
    }
}

impl Default for Color {
    /// Default is fully opaque black.
    fn default() -> Self {
        Self::BLACK
    }
}

// ─── CellColor ───────────────────────────────────────────────────────────────

/// Compact color for terminal cell storage.
///
/// This is what actually gets written to the [`FrameBuffer`] and converted
/// to ANSI escape sequences for terminal output. It's small and fast to
/// compare, optimized for the diff renderer's hot loop.
///
/// For rich color operations, use [`Color`] and convert with
/// [`Color::to_cell_color`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[derive(Default)]
pub enum CellColor {
    /// 24-bit `TrueColor` (the standard for modern terminals).
    Rgb(u8, u8, u8),

    /// ANSI 256-color palette index.
    Ansi256(u8),

    /// Terminal default color (inherits from terminal settings).
    /// Used when the editor should respect the user's terminal theme.
    #[default]
    Default,
}

impl CellColor {
    /// Convert this cell color to sRGB values (0.0–1.0).
    /// Returns `None` for [`CellColor::Default`].
    #[must_use]
    pub fn to_srgb(self) -> Option<(f32, f32, f32)> {
        match self {
            Self::Rgb(r, g, b) => Some((
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
            )),
            Self::Ansi256(idx) => {
                let (r, g, b) = ansi::ansi256_to_rgb(idx);
                Some((
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                ))
            }
            Self::Default => None,
        }
    }

    /// Convert this cell color to a full [`Color`].
    /// Returns `None` for [`CellColor::Default`].
    #[must_use]
    pub fn to_color(self) -> Option<Color> {
        self.to_srgb().map(|(r, g, b)| Color::srgb(r, g, b))
    }

    /// Downgrade to ANSI-256 palette (for terminals without `TrueColor`).
    #[must_use]
    pub fn to_ansi256(self) -> Self {
        match self {
            Self::Rgb(r, g, b) => {
                let color = Color::rgb8(r, g, b);
                Self::Ansi256(color.nearest_ansi256())
            }
            other => other,
        }
    }

    /// Downgrade to ANSI-16 colors (for minimal terminal support).
    #[must_use]
    pub fn to_ansi16(self) -> Self {
        match self {
            Self::Rgb(r, g, b) => {
                let color = Color::rgb8(r, g, b);
                Self::Ansi256(color.nearest_ansi16())
            }
            Self::Ansi256(idx) => {
                let (r, g, b) = ansi::ansi256_to_rgb(idx);
                let color = Color::rgb8(r, g, b);
                Self::Ansi256(color.nearest_ansi16())
            }
            Self::Default => Self::Default,
        }
    }

    /// Whether this is the terminal default color.
    #[inline]
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self, Self::Default)
    }
}

impl fmt::Debug for CellColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Self::Ansi256(idx) => write!(f, "ansi({idx})"),
            Self::Default => write!(f, "default"),
        }
    }
}

impl fmt::Display for CellColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}


impl From<Color> for CellColor {
    fn from(color: Color) -> Self {
        color.to_cell_color()
    }
}

// ─── Color Space Conversion Functions ────────────────────────────────────────
//
// These implement the Oklab color space math created by Björn Ottosson.
// Reference: https://bottosson.github.io/posts/oklab/
//
// Pipeline: OKLCH ↔ Oklab ↔ Linear sRGB ↔ sRGB
//
// All functions are pure, deterministic, and marked #[inline] for the
// compiler to optimize the full pipeline into minimal instructions.

/// Normalize a hue angle to the range [0, 360).
#[inline]
fn normalize_hue(h: f32) -> f32 {
    let h = h % 360.0;
    if h < 0.0 { h + 360.0 } else { h }
}

/// Absolute hue difference (shortest arc on the color wheel).
#[inline]
fn hue_diff(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    if d > 180.0 { 360.0 - d } else { d }
}

/// Interpolate between two hue angles taking the shortest path.
#[inline]
fn interpolate_hue(h1: f32, h2: f32, t: f32) -> f32 {
    let diff = h2 - h1;
    let diff = if diff > 180.0 {
        diff - 360.0
    } else if diff < -180.0 {
        diff + 360.0
    } else {
        diff
    };
    normalize_hue(h1 + diff * t)
}

// ─── OKLCH ↔ Oklab ──────────────────────────────────────────────────────────

/// Convert OKLCH chroma and hue to Oklab a, b components.
#[inline]
fn oklch_to_oklab_ab(c: f32, h: f32) -> (f32, f32) {
    let h_rad = h.to_radians();
    (c * h_rad.cos(), c * h_rad.sin())
}

/// Convert Oklab a, b components to OKLCH chroma and hue.
#[inline]
fn oklab_ab_to_oklch(a: f32, b: f32) -> (f32, f32) {
    let c = a.hypot(b);
    let h = if c < 1e-8 {
        0.0 // Achromatic — hue is undefined, default to 0
    } else {
        let h = b.atan2(a).to_degrees();
        if h < 0.0 { h + 360.0 } else { h }
    };
    (c, h)
}

/// Full OKLCH → Oklab conversion.
#[inline]
fn oklch_to_oklab(l: f32, c: f32, h: f32) -> (f32, f32, f32) {
    let (a, b) = oklch_to_oklab_ab(c, h);
    (l, a, b)
}

/// Full Oklab → OKLCH conversion.
#[inline]
fn oklab_to_oklch(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let (c, h) = oklab_ab_to_oklch(a, b);
    (l, c, h)
}

// ─── Oklab ↔ Linear sRGB ────────────────────────────────────────────────────
//
// The Oklab ↔ Linear sRGB conversion goes through an intermediate LMS
// (Long, Medium, Short cone response) space. The matrices below are from
// Björn Ottosson's original specification.

/// Convert Oklab (L, a, b) to linear sRGB.
#[inline]
fn oklab_to_linear_srgb(l_ok: f32, a: f32, b: f32) -> (f32, f32, f32) {
    // Oklab → LMS (cube roots)
    let l_ = 0.215_803_76f32.mul_add(b, 0.396_337_78f32.mul_add(a, l_ok));
    let m_ = 0.063_854_17f32.mul_add(-b, 0.105_561_346f32.mul_add(-a, l_ok));
    let s_ = 1.291_485_5f32.mul_add(-b, 0.089_484_18f32.mul_add(-a, l_ok));

    // Undo cube root
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    // LMS → Linear sRGB
    let r = 0.230_969_94f32.mul_add(s, 4.076_741_7f32.mul_add(l, -(3.307_711_6 * m)));
    let g = 0.341_319_38f32.mul_add(-s, (-1.268_438f32).mul_add(l, 2.609_757_4 * m));
    let bl = 1.707_614_7f32.mul_add(s, (-0.004_196_086_3f32).mul_add(l, -(0.703_418_6 * m)));

    (r, g, bl)
}

/// Convert linear sRGB to Oklab (L, a, b).
#[inline]
fn linear_srgb_to_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    // Linear sRGB → LMS
    let l = 0.051_445_995f32.mul_add(b, 0.412_221_47f32.mul_add(r, 0.536_332_55 * g));
    let m = 0.107_396_96f32.mul_add(b, 0.211_903_5f32.mul_add(r, 0.680_699_5 * g));
    let s = 0.629_978_7f32.mul_add(b, 0.088_302_46f32.mul_add(r, 0.281_718_84 * g));

    // Cube root (LMS → Oklab intermediate)
    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();

    // Oklab intermediate → Oklab
    let l_ok = 0.004_072_047f32.mul_add(-s_, 0.210_454_26f32.mul_add(l_, 0.793_617_8 * m_));
    let a = 0.450_593_7f32.mul_add(s_, 1.977_998_5f32.mul_add(l_, -(2.428_592_2 * m_)));
    let b_ok = 0.808_675_77f32.mul_add(-s_, 0.025_904_037f32.mul_add(l_, 0.782_771_77 * m_));

    (l_ok, a, b_ok)
}

// ─── Linear sRGB ↔ sRGB (Gamma) ─────────────────────────────────────────────
//
// sRGB uses a piecewise transfer function (gamma curve) to encode linear
// light values into the perceptual domain. Alpha blending MUST happen in
// linear space for physical correctness.

/// Convert a single linear sRGB component to sRGB (apply gamma).
#[inline]
#[must_use]
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055f32.mul_add(c.powf(1.0 / 2.4), -0.055)
    }
}

/// Convert a single sRGB component to linear sRGB (remove gamma).
#[inline]
#[must_use]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// ─── Composite Conversions ───────────────────────────────────────────────────

/// Convert sRGB (0.0–1.0) → OKLCH.
fn srgb_to_oklch(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let lr = srgb_to_linear(r);
    let lg = srgb_to_linear(g);
    let lb = srgb_to_linear(b);
    let (l, a, b_ok) = linear_srgb_to_oklab(lr, lg, lb);
    oklab_to_oklch(l, a, b_ok)
}

/// Convert OKLCH → sRGB (0.0–1.0, may be out of gamut).
fn oklch_to_srgb(l: f32, c: f32, h: f32) -> (f32, f32, f32) {
    let (a, b) = oklch_to_oklab_ab(c, h);
    let (lr, lg, lb) = oklab_to_linear_srgb(l, a, b);
    (linear_to_srgb(lr), linear_to_srgb(lg), linear_to_srgb(lb))
}

// ─── Hex Parsing ─────────────────────────────────────────────────────────────

/// Parse a hex color string into a Color.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#').unwrap_or(s);

    match s.len() {
        // #RGB
        3 => {
            let r = parse_hex_digit(s.as_bytes()[0])?;
            let g = parse_hex_digit(s.as_bytes()[1])?;
            let b = parse_hex_digit(s.as_bytes()[2])?;
            Some(Color::rgb8(r << 4 | r, g << 4 | g, b << 4 | b))
        }
        // #RGBA
        4 => {
            let r = parse_hex_digit(s.as_bytes()[0])?;
            let g = parse_hex_digit(s.as_bytes()[1])?;
            let b = parse_hex_digit(s.as_bytes()[2])?;
            let a = parse_hex_digit(s.as_bytes()[3])?;
            Some(Color::rgba8(r << 4 | r, g << 4 | g, b << 4 | b, a << 4 | a))
        }
        // #RRGGBB
        6 => {
            let r = parse_hex_byte(&s.as_bytes()[0..2])?;
            let g = parse_hex_byte(&s.as_bytes()[2..4])?;
            let b = parse_hex_byte(&s.as_bytes()[4..6])?;
            Some(Color::rgb8(r, g, b))
        }
        // #RRGGBBAA
        8 => {
            let r = parse_hex_byte(&s.as_bytes()[0..2])?;
            let g = parse_hex_byte(&s.as_bytes()[2..4])?;
            let b = parse_hex_byte(&s.as_bytes()[4..6])?;
            let a = parse_hex_byte(&s.as_bytes()[6..8])?;
            Some(Color::rgba8(r, g, b, a))
        }
        _ => None,
    }
}

#[inline]
const fn parse_hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn parse_hex_byte(bytes: &[u8]) -> Option<u8> {
    let hi = parse_hex_digit(bytes[0])?;
    let lo = parse_hex_digit(bytes[1])?;
    Some(hi << 4 | lo)
}

/// Convert a float (0.0–1.0) to a u8 (0–255) with correct rounding.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_u8(v: f32) -> u8 {
    // Safe: clamp guarantees 0.0 <= value <= 255.0 before truncation.
    v.mul_add(255.0, 0.5).clamp(0.0, 255.0) as u8
}

// ─── ANSI Palette ────────────────────────────────────────────────────────────

pub mod ansi {
    //! ANSI color palette definitions and perceptual nearest-match.
    //!
    //! The 256-color ANSI palette consists of:
    //! - Colors 0–7: Standard colors (black, red, green, yellow, blue, magenta, cyan, white)
    //! - Colors 8–15: Bright variants of the standard colors
    //! - Colors 16–231: A 6×6×6 RGB color cube
    //! - Colors 232–255: A 24-step grayscale ramp

    use super::{Color, srgb_to_oklch, oklch_to_oklab};

    /// The standard ANSI-16 palette as RGB values.
    ///
    /// These match the widely-used "xterm" defaults. Individual terminals
    /// may override these, but for nearest-match calculations these provide
    /// a reasonable reference.
    pub const ANSI16_RGB: [(u8, u8, u8); 16] = [
        (0, 0, 0),       // 0: Black
        (128, 0, 0),     // 1: Red
        (0, 128, 0),     // 2: Green
        (128, 128, 0),   // 3: Yellow
        (0, 0, 128),     // 4: Blue
        (128, 0, 128),   // 5: Magenta
        (0, 128, 128),   // 6: Cyan
        (192, 192, 192), // 7: White
        (128, 128, 128), // 8: Bright Black
        (255, 0, 0),     // 9: Bright Red
        (0, 255, 0),     // 10: Bright Green
        (255, 255, 0),   // 11: Bright Yellow
        (0, 0, 255),     // 12: Bright Blue
        (255, 0, 255),   // 13: Bright Magenta
        (0, 255, 255),   // 14: Bright Cyan
        (255, 255, 255), // 15: Bright White
    ];

    /// Convert an ANSI-256 palette index to RGB values.
    #[must_use]
    pub fn ansi256_to_rgb(idx: u8) -> (u8, u8, u8) {
        match idx {
            // Standard + bright colors
            0..=15 => ANSI16_RGB[idx as usize],

            // 6×6×6 color cube (indices 16–231)
            16..=231 => {
                let idx = idx - 16;
                let r_idx = idx / 36;
                let g_idx = (idx % 36) / 6;
                let b_idx = idx % 6;

                // The cube uses: 0, 95, 135, 175, 215, 255
                let to_value = |i: u8| -> u8 {
                    if i == 0 { 0 } else { 55 + 40 * i }
                };

                (to_value(r_idx), to_value(g_idx), to_value(b_idx))
            }

            // Grayscale ramp (indices 232–255)
            232..=255 => {
                let v = 8 + 10 * (idx - 232);
                (v, v, v)
            }
        }
    }

    /// Convert an ANSI-256 palette index to a [`Color`].
    #[must_use]
    pub fn ansi256_to_color(idx: u8) -> Color {
        let (r, g, b) = ansi256_to_rgb(idx);
        Color::rgb8(r, g, b)
    }

    /// Find the nearest ANSI-256 color using perceptual distance (Oklab Delta E).
    ///
    /// This produces much better matches than naive Euclidean RGB distance,
    /// especially for colors with similar lightness but different hues.
    #[must_use]
    pub fn nearest_ansi256(color: Color) -> u8 {
        let (l1, a1, b1) = oklch_to_oklab(color.l, color.c, color.h);

        let mut best_idx: u8 = 0;
        let mut best_dist = f32::MAX;

        for idx in 0u8..=255 {
            let (r, g, b) = ansi256_to_rgb(idx);
            let (l2, c2, h2) = srgb_to_oklch(
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
            );
            let (_, a2, b2) = oklch_to_oklab(l2, c2, h2);

            let dl = l1 - l2;
            let da = a1 - a2;
            let db = b1 - b2;
            let dist = db.mul_add(db, dl.mul_add(dl, da * da));

            if dist < best_dist {
                best_dist = dist;
                best_idx = idx;
            }
        }

        best_idx
    }

    /// Find the nearest ANSI-16 color using perceptual distance.
    #[must_use]
    pub fn nearest_ansi16(color: Color) -> u8 {
        let (l1, a1, b1) = oklch_to_oklab(color.l, color.c, color.h);

        let mut best_idx: u8 = 0;
        let mut best_dist = f32::MAX;

        for idx in 0u8..16 {
            let (r, g, b) = ANSI16_RGB[idx as usize];
            let (l2, c2, h2) = srgb_to_oklch(
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
            );
            let (_, a2, b2) = oklch_to_oklab(l2, c2, h2);

            let dl = l1 - l2;
            let da = a1 - a2;
            let db = b1 - b2;
            let dist = db.mul_add(db, dl.mul_add(dl, da * da));

            if dist < best_dist {
                best_dist = dist;
                best_idx = idx;
            }
        }

        best_idx
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: check that two f32 values are approximately equal.
    fn approx_eq(a: f32, b: f32, epsilon: f32) -> bool {
        (a - b).abs() < epsilon
    }

    // Helper: assert RGB values are close (within ±1 out of 255).
    fn assert_rgb8_close(actual: (u8, u8, u8), expected: (u8, u8, u8)) {
        let (ar, ag, ab) = actual;
        let (er, eg, eb) = expected;
        assert!(
            (i16::from(ar) - i16::from(er)).unsigned_abs() <= 1
                && (i16::from(ag) - i16::from(eg)).unsigned_abs() <= 1
                && (i16::from(ab) - i16::from(eb)).unsigned_abs() <= 1,
            "RGB mismatch: got ({ar}, {ag}, {ab}), expected ({er}, {eg}, {eb})"
        );
    }

    // ── Roundtrip Tests ──────────────────────────────────────────────────

    #[test]
    fn srgb_to_oklch_roundtrip() {
        // Test a variety of colors roundtrip: sRGB → OKLCH → sRGB
        let test_colors: [(f32, f32, f32); 8] = [
            (1.0, 0.0, 0.0), // Red
            (0.0, 1.0, 0.0), // Green
            (0.0, 0.0, 1.0), // Blue
            (1.0, 1.0, 0.0), // Yellow
            (0.0, 1.0, 1.0), // Cyan
            (1.0, 0.0, 1.0), // Magenta
            (1.0, 1.0, 1.0), // White
            (0.0, 0.0, 0.0), // Black
        ];

        for (r, g, b) in test_colors {
            let color = Color::srgb(r, g, b);
            let (rr, rg, rb) = color.to_srgb();
            assert!(
                approx_eq(r, rr, 0.005)
                    && approx_eq(g, rg, 0.005)
                    && approx_eq(b, rb, 0.005),
                "Roundtrip failed for ({r}, {g}, {b}): got ({rr:.4}, {rg:.4}, {rb:.4})"
            );
        }
    }

    #[test]
    fn oklch_identity_roundtrip() {
        // Create from OKLCH, convert to sRGB and back, verify OKLCH values.
        // Note: roundtrip precision is limited by sRGB gamma curve quantization.
        // We use a moderate chroma to stay well within gamut.
        let original = Color::oklch(0.7, 0.10, 90.0);
        let (r, g, b) = original.to_srgb();
        let recovered = Color::srgb(r, g, b);

        assert!(
            approx_eq(original.l, recovered.l, 0.02),
            "L mismatch: {} vs {}",
            original.l,
            recovered.l
        );
        assert!(
            approx_eq(original.c, recovered.c, 0.02),
            "C mismatch: {} vs {}",
            original.c,
            recovered.c
        );
        assert!(
            hue_diff(original.h, recovered.h) < 2.0,
            "H mismatch: {} vs {}",
            original.h,
            recovered.h
        );
    }

    // ── Hex Parsing ──────────────────────────────────────────────────────

    #[test]
    fn hex_parsing_rrggbb() {
        let color = Color::hex("#ff8000").unwrap();
        let (r, g, b) = color.to_rgb8();
        assert_rgb8_close((r, g, b), (255, 128, 0));
        assert!(color.is_opaque());
    }

    #[test]
    fn hex_parsing_short() {
        let color = Color::hex("#f80").unwrap();
        let (r, g, b) = color.to_rgb8();
        assert_rgb8_close((r, g, b), (255, 136, 0));
    }

    #[test]
    fn hex_parsing_with_alpha() {
        let color = Color::hex("#ff000080").unwrap();
        assert!(approx_eq(color.alpha, 128.0 / 255.0, 0.01));
    }

    #[test]
    fn hex_parsing_no_hash() {
        let color = Color::hex("00ff00").unwrap();
        let (r, g, b) = color.to_rgb8();
        assert_rgb8_close((r, g, b), (0, 255, 0));
    }

    #[test]
    fn hex_parsing_invalid() {
        assert!(Color::hex("xyz").is_none());
        assert!(Color::hex("#12345").is_none());
        assert!(Color::hex("").is_none());
    }

    #[test]
    fn hex_roundtrip() {
        let original = "#c86432";
        let color = Color::hex(original).unwrap();
        let hex = color.to_hex();
        assert_eq!(hex, original);
    }

    // ── Known Values ─────────────────────────────────────────────────────

    #[test]
    fn black_is_zero_lightness() {
        let black = Color::srgb(0.0, 0.0, 0.0);
        assert!(approx_eq(black.l, 0.0, 0.001));
        assert!(approx_eq(black.c, 0.0, 0.001));
    }

    #[test]
    fn white_is_full_lightness() {
        let white = Color::srgb(1.0, 1.0, 1.0);
        assert!(approx_eq(white.l, 1.0, 0.001));
        assert!(approx_eq(white.c, 0.0, 0.001));
    }

    #[test]
    fn gray_has_no_chroma() {
        let gray = Color::srgb(0.5, 0.5, 0.5);
        assert!(gray.is_achromatic());
    }

    #[test]
    fn red_has_hue_near_30() {
        // Pure sRGB red maps to roughly hue 29° in OKLCH
        let red = Color::srgb(1.0, 0.0, 0.0);
        assert!(red.h > 20.0 && red.h < 35.0, "Red hue was {}", red.h);
        assert!(red.c > 0.2, "Red chroma was {}", red.c);
    }

    // ── Perceptual Operations ────────────────────────────────────────────

    #[test]
    fn lighten_increases_lightness() {
        let color = Color::oklch(0.5, 0.1, 90.0);
        let lighter = color.lighten(0.2);
        assert!(approx_eq(lighter.l, 0.7, 0.001));
        assert!(approx_eq(lighter.c, color.c, 0.001)); // Chroma unchanged
        assert!(approx_eq(lighter.h, color.h, 0.001)); // Hue unchanged
    }

    #[test]
    fn darken_decreases_lightness() {
        let color = Color::oklch(0.5, 0.1, 90.0);
        let darker = color.darken(0.3);
        assert!(approx_eq(darker.l, 0.2, 0.001));
    }

    #[test]
    fn lighten_clamps_to_one() {
        let color = Color::oklch(0.9, 0.1, 90.0);
        let lighter = color.lighten(0.5);
        assert!(approx_eq(lighter.l, 1.0, 0.001));
    }

    #[test]
    fn darken_clamps_to_zero() {
        let color = Color::oklch(0.1, 0.1, 90.0);
        let darker = color.darken(0.5);
        assert!(approx_eq(darker.l, 0.0, 0.001));
    }

    #[test]
    fn shift_hue_wraps() {
        let color = Color::oklch(0.5, 0.1, 350.0);
        let shifted = color.shift_hue(30.0);
        assert!(approx_eq(shifted.h, 20.0, 0.001));
    }

    #[test]
    fn shift_hue_negative_wraps() {
        let color = Color::oklch(0.5, 0.1, 10.0);
        let shifted = color.shift_hue(-30.0);
        assert!(approx_eq(shifted.h, 340.0, 0.001));
    }

    #[test]
    fn complement_is_180_degrees() {
        let color = Color::oklch(0.5, 0.1, 60.0);
        let comp = color.complement();
        assert!(approx_eq(comp.h, 240.0, 0.001));
    }

    #[test]
    fn saturate_increases_chroma() {
        let color = Color::oklch(0.5, 0.1, 90.0);
        let vivid = color.saturate(0.05);
        assert!(approx_eq(vivid.c, 0.15, 0.001));
    }

    #[test]
    fn desaturate_decreases_chroma() {
        let color = Color::oklch(0.5, 0.1, 90.0);
        let muted = color.desaturate(0.05);
        assert!(approx_eq(muted.c, 0.05, 0.001));
    }

    #[test]
    fn desaturate_clamps_to_zero() {
        let color = Color::oklch(0.5, 0.05, 90.0);
        let muted = color.desaturate(0.1);
        assert!(approx_eq(muted.c, 0.0, 0.001));
    }

    // ── Mix / Interpolation ──────────────────────────────────────────────

    #[test]
    fn mix_at_zero_returns_self() {
        let a = Color::oklch(0.3, 0.1, 30.0);
        let b = Color::oklch(0.7, 0.2, 270.0);
        let mixed = a.mix(&b, 0.0);
        assert!(approx_eq(mixed.l, a.l, 0.001));
        assert!(approx_eq(mixed.c, a.c, 0.001));
    }

    #[test]
    fn mix_at_one_returns_other() {
        let a = Color::oklch(0.3, 0.1, 30.0);
        let b = Color::oklch(0.7, 0.2, 270.0);
        let mixed = a.mix(&b, 1.0);
        assert!(approx_eq(mixed.l, b.l, 0.001));
        assert!(approx_eq(mixed.c, b.c, 0.001));
    }

    #[test]
    fn mix_at_half_is_midpoint() {
        let a = Color::oklch(0.3, 0.1, 0.0);
        let b = Color::oklch(0.7, 0.3, 0.0);
        let mixed = a.mix(&b, 0.5);
        assert!(approx_eq(mixed.l, 0.5, 0.001));
        assert!(approx_eq(mixed.c, 0.2, 0.001));
    }

    #[test]
    fn mix_hue_takes_shortest_path() {
        // From 10° to 350° should go through 0°, not through 180°
        let a = Color::oklch(0.5, 0.1, 10.0);
        let b = Color::oklch(0.5, 0.1, 350.0);
        let mixed = a.mix(&b, 0.5);
        // Midpoint should be at 0° (or 360°)
        assert!(
            mixed.h < 5.0 || mixed.h > 355.0,
            "Expected hue near 0/360, got {}",
            mixed.h
        );
    }

    // ── Alpha Blending ───────────────────────────────────────────────────

    #[test]
    fn blend_opaque_over_anything() {
        let src = Color::srgb(1.0, 0.0, 0.0); // Opaque red
        let dst = Color::srgb(0.0, 0.0, 1.0); // Opaque blue
        let result = src.blend_over(&dst);
        let (r, g, b) = result.to_srgb();
        assert!(approx_eq(r, 1.0, 0.01));
        assert!(approx_eq(g, 0.0, 0.01));
        assert!(approx_eq(b, 0.0, 0.01));
    }

    #[test]
    fn blend_transparent_shows_background() {
        let src = Color::TRANSPARENT;
        let dst = Color::srgb(0.0, 1.0, 0.0);
        let result = src.blend_over(&dst);
        let (r, g, b) = result.to_srgb();
        assert!(approx_eq(r, 0.0, 0.01));
        assert!(approx_eq(g, 1.0, 0.01));
        assert!(approx_eq(b, 0.0, 0.01));
    }

    #[test]
    fn blend_half_alpha_mixes() {
        let src = Color::srgb(1.0, 0.0, 0.0).with_alpha(0.5);
        let dst = Color::srgb(0.0, 0.0, 1.0);
        let result = src.blend_over(&dst);

        // Result should be a purple-ish color (red + blue)
        let (r, _, b) = result.to_srgb();
        assert!(r > 0.3, "Expected red component > 0.3, got {r}");
        assert!(b > 0.3, "Expected blue component > 0.3, got {b}");
        assert!(result.is_opaque());
    }

    #[test]
    fn blend_preserves_alpha_composition() {
        // Two 50% layers over nothing should produce ~75% opacity
        let a = Color::srgb(1.0, 0.0, 0.0).with_alpha(0.5);
        let b = Color::srgb(0.0, 0.0, 1.0).with_alpha(0.5);
        let result = a.blend_over(&b);
        assert!(
            approx_eq(result.alpha, 0.75, 0.01),
            "Expected alpha ~0.75, got {}",
            result.alpha
        );
    }

    // ── Distance ─────────────────────────────────────────────────────────

    #[test]
    fn identical_colors_have_zero_distance() {
        let a = Color::oklch(0.5, 0.1, 90.0);
        let b = Color::oklch(0.5, 0.1, 90.0);
        assert!(a.distance(&b) < 0.001);
    }

    #[test]
    fn black_white_have_large_distance() {
        let dist = Color::BLACK.distance(&Color::WHITE);
        assert!(dist > 0.9, "Black-white distance was {dist}");
    }

    #[test]
    fn similar_colors_have_small_distance() {
        let a = Color::oklch(0.5, 0.1, 90.0);
        let b = Color::oklch(0.51, 0.1, 91.0);
        assert!(
            a.distance(&b) < 0.02,
            "Similar colors distance was {}",
            a.distance(&b)
        );
    }

    // ── Gamut Mapping ────────────────────────────────────────────────────

    #[test]
    fn in_gamut_colors_unchanged() {
        // Use a color derived from sRGB to guarantee it's in gamut
        let color = Color::srgb(0.4, 0.6, 0.5);
        assert!(color.in_srgb_gamut());
        let mapped = color.to_gamut();
        assert!(approx_eq(color.c, mapped.c, 0.001));
    }

    #[test]
    fn out_of_gamut_reduced_to_fit() {
        // Very high chroma at some hues will be out of gamut
        let color = Color::oklch(0.5, 0.4, 180.0);
        if !color.in_srgb_gamut() {
            let mapped = color.to_gamut();
            assert!(mapped.in_srgb_gamut());
            assert!(mapped.c < color.c);
            assert!(approx_eq(mapped.l, color.l, 0.001)); // Lightness preserved
            assert!(approx_eq(mapped.h, color.h, 0.5)); // Hue preserved
        }
    }

    // ── Resolve (Transparency Bridge) ────────────────────────────────────

    #[test]
    fn resolve_opaque_ignores_background() {
        let color = Color::srgb(1.0, 0.0, 0.0); // Opaque red
        let bg = CellColor::Rgb(0, 0, 255); // Blue background
        let resolved = color.resolve_over(&bg);
        assert_eq!(resolved, CellColor::Rgb(255, 0, 0));
    }

    #[test]
    fn resolve_transparent_returns_background() {
        let color = Color::TRANSPARENT;
        let bg = CellColor::Rgb(0, 255, 0);
        let resolved = color.resolve_over(&bg);
        assert_eq!(resolved, bg);
    }

    #[test]
    fn resolve_transparent_preserves_default_bg() {
        let color = Color::TRANSPARENT;
        let bg = CellColor::Default;
        let resolved = color.resolve_over(&bg);
        assert_eq!(resolved, CellColor::Default);
    }

    #[test]
    fn resolve_semi_transparent_blends() {
        let color = Color::srgba(1.0, 0.0, 0.0, 0.5); // 50% red
        let bg = CellColor::Rgb(0, 0, 255); // Blue
        let resolved = color.resolve_over(&bg);

        // Should be a blended purple-ish color (not pure red, not pure blue)
        if let CellColor::Rgb(r, _, b) = resolved {
            assert!(r > 100, "Expected red > 100, got {r}");
            assert!(b > 100, "Expected blue > 100, got {b}");
        } else {
            panic!("Expected Rgb variant");
        }
    }

    #[test]
    fn resolve_over_default_bg_treats_as_black() {
        let color = Color::srgba(1.0, 1.0, 1.0, 0.5); // 50% white
        let resolved_default = color.resolve_over(&CellColor::Default);
        let resolved_black = color.resolve_over(&CellColor::Rgb(0, 0, 0));

        // Both should produce similar results (default → black fallback)
        assert_eq!(resolved_default, resolved_black);
    }

    #[test]
    fn resolve_convenience_composites_over_black() {
        let color = Color::srgba(1.0, 1.0, 1.0, 0.5);
        let resolved = color.resolve();
        let manual = color.resolve_over(&CellColor::Rgb(0, 0, 0));
        assert_eq!(resolved, manual);
    }

    #[test]
    fn resolve_opaque_convenience() {
        let color = Color::srgb(0.5, 0.5, 0.5);
        let resolved = color.resolve();
        assert_eq!(resolved, color.to_cell_color());
    }

    // ── CellColor ────────────────────────────────────────────────────────

    #[test]
    fn cell_color_from_color() {
        let color = Color::hex("#ff8040").unwrap();
        let cell = color.to_cell_color();
        assert_eq!(cell, CellColor::Rgb(255, 128, 64));
    }

    #[test]
    fn cell_color_default() {
        let cell = CellColor::Default;
        assert!(cell.is_default());
        assert!(cell.to_srgb().is_none());
        assert!(cell.to_color().is_none());
    }

    #[test]
    fn cell_color_debug_format() {
        assert_eq!(format!("{:?}", CellColor::Rgb(255, 128, 0)), "#ff8000");
        assert_eq!(format!("{:?}", CellColor::Ansi256(42)), "ansi(42)");
        assert_eq!(format!("{:?}", CellColor::Default), "default");
    }

    // ── ANSI Palette ─────────────────────────────────────────────────────

    #[test]
    fn ansi_standard_colors() {
        // Black
        assert_eq!(ansi::ansi256_to_rgb(0), (0, 0, 0));
        // Bright white
        assert_eq!(ansi::ansi256_to_rgb(15), (255, 255, 255));
    }

    #[test]
    fn ansi_color_cube() {
        // First cube entry (index 16) = (0, 0, 0)
        assert_eq!(ansi::ansi256_to_rgb(16), (0, 0, 0));
        // Last cube entry (index 231) = (255, 255, 255)
        assert_eq!(ansi::ansi256_to_rgb(231), (255, 255, 255));
        // Pure red in cube (index 196) = (255, 0, 0)
        assert_eq!(ansi::ansi256_to_rgb(196), (255, 0, 0));
    }

    #[test]
    fn ansi_grayscale() {
        // First gray (index 232) = (8, 8, 8)
        assert_eq!(ansi::ansi256_to_rgb(232), (8, 8, 8));
        // Last gray (index 255) = (238, 238, 238)
        assert_eq!(ansi::ansi256_to_rgb(255), (238, 238, 238));
    }

    #[test]
    fn nearest_ansi16_finds_close_match() {
        // Pure red should map to ANSI bright red (index 9)
        let red = Color::srgb(1.0, 0.0, 0.0);
        let idx = red.nearest_ansi16();
        assert_eq!(idx, 9, "Pure red should match bright red (9), got {idx}");
    }

    #[test]
    fn nearest_ansi16_black_and_white() {
        assert_eq!(Color::BLACK.nearest_ansi16(), 0);
        assert_eq!(Color::WHITE.nearest_ansi16(), 15);
    }

    #[test]
    fn nearest_ansi256_pure_colors() {
        // Pure white should match index 15 or 231 or 255-area
        let white_idx = Color::WHITE.nearest_ansi256();
        let (r, g, b) = ansi::ansi256_to_rgb(white_idx);
        assert_eq!((r, g, b), (255, 255, 255));
    }

    // ── Equality ─────────────────────────────────────────────────────────

    #[test]
    fn color_equality_with_epsilon() {
        let a = Color::oklch(0.5, 0.1, 90.0);
        let b = Color::oklch(0.5, 0.1, 90.0);
        assert_eq!(a, b);
    }

    #[test]
    fn color_equality_achromatic_ignores_hue() {
        // Gray colors should be equal regardless of hue
        let a = Color::gray(0.5);
        let b = Color::oklch(0.5, 0.0, 180.0);
        assert_eq!(a, b);
    }

    // ── Display / Debug ──────────────────────────────────────────────────

    #[test]
    fn color_display_hex() {
        let red = Color::srgb(1.0, 0.0, 0.0);
        assert_eq!(format!("{red}"), "#ff0000");
    }

    #[test]
    fn color_debug_format() {
        let color = Color::oklch(0.5, 0.1, 90.0);
        let dbg = format!("{color:?}");
        assert!(dbg.starts_with("Color::oklch("));
    }
}
