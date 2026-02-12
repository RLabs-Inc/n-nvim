//! Sacred Geometry pattern engine — pure mathematical hue generation.
//!
//! Each pattern takes a `base_hue` (0-360) and generates a set of harmonious
//! hue angles using a specific mathematical relationship. The first hue in
//! the result is always the `base_hue` itself.

/// The kind of Sacred Geometry pattern used to generate hue arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternKind {
    /// Golden angle (137.508) spacing — nature's favorite.
    GoldenRatio,
    /// Fibonacci sequence reciprocal spacing.
    Fibonacci,
    /// 120-degree spacing (3 colors).
    Triadic,
    /// Complement +/- 30 degrees (4 colors).
    SplitComplementary,
    /// Adjacent hues +/- 30, +/- 60 (5 colors).
    Analogous,
    /// 90-degree spacing (4 colors + variants).
    Tetradic,
    /// 72-degree spacing (5 colors + base).
    Pentagram,
    /// 60-degree spacing (6 colors + base).
    Hexagram,
    /// Two offset triads (6 + base).
    Merkaba,
    /// 6-fold symmetry with golden subdivisions.
    FlowerOfLife,
    /// 13 circles of Metatron's cube.
    Metatron,
    /// Phi/e/sqrt spiral angles.
    SacredSpirals,
    /// 9-fold symmetry.
    SriYantra,
    /// Parametric surface angles.
    Torus,
    /// All 6 solfeggio frequencies + overtones.
    SolfeggioAll,
    /// Phi^n forward/reverse/mean.
    DivineProportion,
    /// Hexagonal + outer + intersection.
    SeedOfLife,
    /// Harmonic series + cymatics.
    HarmonicResonance,
    /// 5x5 grid with phi multipliers.
    PhiGrid,
    /// Single hue only.
    Monochromatic,
}

impl PatternKind {
    /// Generate an array of hue angles from this pattern.
    ///
    /// The first element is always `base_hue`. All values are in [0, 360).
    #[must_use]
    pub fn generate(self, base_hue: f32) -> Vec<f32> {
        generate(self, base_hue)
    }

    /// Generate a cohesive subset (first 5 hues) for simpler palettes.
    #[must_use]
    pub fn generate_few(self, base_hue: f32) -> Vec<f32> {
        let mut hues = generate(self, base_hue);
        hues.truncate(5);
        hues
    }

    /// Human-readable name of this pattern.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GoldenRatio => "golden-ratio",
            Self::Fibonacci => "fibonacci",
            Self::Triadic => "triadic",
            Self::SplitComplementary => "split-complementary",
            Self::Analogous => "analogous",
            Self::Tetradic => "tetradic",
            Self::Pentagram => "pentagram",
            Self::Hexagram => "hexagram",
            Self::Merkaba => "merkaba",
            Self::FlowerOfLife => "flower-of-life",
            Self::Metatron => "metatron",
            Self::SacredSpirals => "sacred-spirals",
            Self::SriYantra => "sri-yantra",
            Self::Torus => "torus",
            Self::SolfeggioAll => "solfeggio",
            Self::DivineProportion => "divine-proportion",
            Self::SeedOfLife => "seed-of-life",
            Self::HarmonicResonance => "harmonic-resonance",
            Self::PhiGrid => "phi-grid",
            Self::Monochromatic => "monochromatic",
        }
    }

    /// Parse a pattern from its name string (case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        Self::all().iter().find(|p| p.name() == lower).copied()
    }

    /// All available pattern kinds.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::GoldenRatio, Self::Fibonacci, Self::Triadic,
            Self::SplitComplementary, Self::Analogous, Self::Tetradic,
            Self::Pentagram, Self::Hexagram, Self::Merkaba,
            Self::FlowerOfLife, Self::Metatron, Self::SacredSpirals,
            Self::SriYantra, Self::Torus, Self::SolfeggioAll,
            Self::DivineProportion, Self::SeedOfLife, Self::HarmonicResonance,
            Self::PhiGrid, Self::Monochromatic,
        ]
    }
}

/// Normalize a hue to [0, 360).
fn norm(h: f32) -> f32 {
    let h = h % 360.0;
    if h < 0.0 { h + 360.0 } else { h }
}

/// Core pattern generation dispatch.
fn generate(kind: PatternKind, base: f32) -> Vec<f32> {
    match kind {
        PatternKind::GoldenRatio => {
            // Golden angle = 360 / phi^2 ≈ 137.508
            const GOLDEN_ANGLE: f32 = 137.507_76;
            (0..8).map(|i| norm((i as f32).mul_add(GOLDEN_ANGLE, base))).collect()
        }
        PatternKind::Fibonacci => {
            let fibs: [f32; 10] = [1.0, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0, 55.0];
            fibs.iter().map(|&f| norm(base + 360.0 / f)).collect()
        }
        PatternKind::Triadic => {
            vec![norm(base), norm(base + 120.0), norm(base + 240.0), norm(base + 60.0)]
        }
        PatternKind::SplitComplementary => {
            vec![norm(base), norm(base + 150.0), norm(base + 180.0), norm(base + 210.0)]
        }
        PatternKind::Analogous => {
            vec![
                norm(base), norm(base + 30.0), norm(base - 30.0),
                norm(base + 60.0), norm(base - 60.0),
            ]
        }
        PatternKind::Tetradic => {
            vec![
                norm(base), norm(base + 90.0), norm(base + 180.0),
                norm(base + 270.0), norm(base + 45.0),
            ]
        }
        PatternKind::Pentagram => {
            (0..6).map(|i| norm((i as f32).mul_add(72.0, base))).collect()
        }
        PatternKind::Hexagram => {
            (0..7).map(|i| norm((i as f32).mul_add(60.0, base))).collect()
        }
        PatternKind::Merkaba => {
            // Two triads offset by 30 degrees.
            let triad_a = [norm(base), norm(base + 120.0), norm(base + 240.0)];
            let triad_b = [norm(base + 30.0), norm(base + 150.0), norm(base + 270.0)];
            let mut v = Vec::with_capacity(7);
            v.extend_from_slice(&triad_a);
            v.extend_from_slice(&triad_b);
            v.push(norm(base + 60.0));
            v
        }
        PatternKind::FlowerOfLife => {
            // 6-fold symmetry + golden angle subdivisions.
            const FLOWER_GA: f32 = 137.507_76;
            let mut v = Vec::with_capacity(13);
            for i in 0..6 {
                v.push(norm((i as f32).mul_add(60.0, base)));
            }
            for i in 0..7 {
                v.push(norm((i as f32 * FLOWER_GA).mul_add(0.5, base + 30.0)));
            }
            v
        }
        PatternKind::Metatron => {
            // 13 circles: center + 6 inner (60 apart) + 6 outer (60 apart, offset 30).
            let mut v = Vec::with_capacity(14);
            v.push(norm(base));
            for i in 0..6 {
                v.push(norm((i as f32).mul_add(60.0, base) + 15.0));
            }
            for i in 0..6 {
                v.push(norm((i as f32).mul_add(60.0, base) + 45.0));
            }
            v.push(norm(base + 180.0));
            v
        }
        PatternKind::SacredSpirals => {
            // Phi spiral, e spiral, sqrt(2) spiral.
            const PHI: f32 = 1.618_034;
            const EULER: f32 = std::f32::consts::E;
            const SQRT2: f32 = std::f32::consts::SQRT_2;
            let mut v = Vec::with_capacity(13);
            v.push(norm(base));
            for i in 1..5 {
                v.push(norm(base + i as f32 * 360.0 / PHI));
            }
            for i in 1..5 {
                v.push(norm(base + i as f32 * 360.0 / EULER));
            }
            for i in 1..5 {
                v.push(norm(base + i as f32 * 360.0 / SQRT2));
            }
            v.truncate(13);
            v
        }
        PatternKind::SriYantra => {
            // 9-fold symmetry + interlocking triangles.
            (0..10).map(|i| norm((i as f32).mul_add(40.0, base))).collect()
        }
        PatternKind::Torus => {
            // Parametric surface angles: golden angle along the torus surface.
            const GA: f32 = 137.507_76;
            (0..10).map(|i| {
                let theta = i as f32 * GA;
                let phi_val = i as f32 * 60.0;
                norm(theta.mul_add(0.6, base) + phi_val * 0.4)
            }).collect()
        }
        PatternKind::SolfeggioAll => {
            // Solfeggio frequencies mapped to hue: 396, 417, 528, 639, 741, 852 Hz.
            let freqs: [f32; 6] = [396.0, 417.0, 528.0, 639.0, 741.0, 852.0];
            let mut v = Vec::with_capacity(15);
            v.push(norm(base));
            for &freq in &freqs {
                v.push(norm(base + (freq % 360.0)));
            }
            // Overtones: double the frequency mod 360.
            for &freq in &freqs {
                v.push(norm(base + (freq * 2.0 % 360.0)));
            }
            // Sub-harmonics: half the frequency.
            v.push(norm(base + (freqs[0] * 0.5 % 360.0)));
            v.push(norm(base + (freqs[3] * 0.5 % 360.0)));
            v.truncate(15);
            v
        }
        PatternKind::DivineProportion => {
            // Phi^n forward, reverse, and geometric mean.
            const PHI: f32 = 1.618_034;
            let mut v = Vec::with_capacity(10);
            v.push(norm(base));
            let mut phi_pow = PHI;
            for _ in 0..3 {
                v.push(norm(base + 360.0 / phi_pow));
                v.push(norm(base - 360.0 / phi_pow));
                phi_pow *= PHI;
            }
            v.push(norm(360.0f32.mul_add(PHI - 1.0, base)));
            v.push(norm(base + 180.0 / PHI));
            v.push(norm(base + 360.0 / (PHI * PHI)));
            v.truncate(10);
            v
        }
        PatternKind::SeedOfLife => {
            // Hexagonal + outer ring + intersection points.
            let mut v = Vec::with_capacity(19);
            v.push(norm(base));
            // Inner hex (6).
            for i in 0..6 { v.push(norm((i as f32).mul_add(60.0, base) + 10.0)); }
            // Outer hex (6).
            for i in 0..6 { v.push(norm((i as f32).mul_add(60.0, base) + 30.0)); }
            // Intersection points (6).
            for i in 0..6 { v.push(norm((i as f32).mul_add(60.0, base) + 50.0)); }
            v
        }
        PatternKind::HarmonicResonance => {
            // Harmonic series: base * n, mapped into [0, 360).
            let mut v = Vec::with_capacity(17);
            v.push(norm(base));
            for n in 2..=12 {
                v.push(norm(base * n as f32));
            }
            // Cymatics nodes: standing wave patterns.
            for n in 2..=6 {
                v.push(norm(base + 360.0 / n as f32));
            }
            v.truncate(17);
            v
        }
        PatternKind::PhiGrid => {
            // 5x5 grid points using phi multipliers.
            const PHI: f32 = 1.618_034;
            let mut v = Vec::with_capacity(13);
            v.push(norm(base));
            for row in 0..3 {
                for col in 0..4 {
                    let h = (row as f32).mul_add(PHI, col as f32).mul_add(30.0, base);
                    v.push(norm(h));
                }
            }
            v
        }
        PatternKind::Monochromatic => {
            vec![norm(base)]
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// All patterns return at least one hue.
    #[test]
    fn all_patterns_non_empty() {
        let all = [
            PatternKind::GoldenRatio, PatternKind::Fibonacci,
            PatternKind::Triadic, PatternKind::SplitComplementary,
            PatternKind::Analogous, PatternKind::Tetradic,
            PatternKind::Pentagram, PatternKind::Hexagram,
            PatternKind::Merkaba, PatternKind::FlowerOfLife,
            PatternKind::Metatron, PatternKind::SacredSpirals,
            PatternKind::SriYantra, PatternKind::Torus,
            PatternKind::SolfeggioAll, PatternKind::DivineProportion,
            PatternKind::SeedOfLife, PatternKind::HarmonicResonance,
            PatternKind::PhiGrid, PatternKind::Monochromatic,
        ];
        for kind in all {
            let hues = kind.generate(270.0);
            assert!(!hues.is_empty(), "{kind:?} returned empty");
        }
    }

    /// First hue is derived from base_hue.
    #[test]
    fn first_hue_is_base() {
        let base = 120.0;
        // Most patterns start with norm(base) or norm(base + something small).
        // Monochromatic is the simplest to test exactly.
        let hues = PatternKind::Monochromatic.generate(base);
        assert!((hues[0] - base).abs() < 0.01);
    }

    /// All hues are in [0, 360).
    #[test]
    fn all_hues_in_range() {
        let all = [
            PatternKind::GoldenRatio, PatternKind::Fibonacci,
            PatternKind::Triadic, PatternKind::Merkaba,
            PatternKind::FlowerOfLife, PatternKind::SolfeggioAll,
            PatternKind::SeedOfLife, PatternKind::Monochromatic,
        ];
        for kind in all {
            for base in [0.0, 90.0, 180.0, 270.0, 359.9] {
                for h in kind.generate(base) {
                    assert!(
                        (0.0..360.0).contains(&h),
                        "{kind:?} base={base} produced hue {h}"
                    );
                }
            }
        }
    }

    /// Patterns are deterministic.
    #[test]
    fn deterministic() {
        let a = PatternKind::GoldenRatio.generate(42.0);
        let b = PatternKind::GoldenRatio.generate(42.0);
        assert_eq!(a, b);
    }

    /// GoldenRatio produces 8 hues.
    #[test]
    fn golden_ratio_count() {
        assert_eq!(PatternKind::GoldenRatio.generate(0.0).len(), 8);
    }

    /// Fibonacci produces 10 hues.
    #[test]
    fn fibonacci_count() {
        assert_eq!(PatternKind::Fibonacci.generate(0.0).len(), 10);
    }

    /// Triadic produces 4 hues.
    #[test]
    fn triadic_count() {
        assert_eq!(PatternKind::Triadic.generate(0.0).len(), 4);
    }

    /// Merkaba produces 7 hues.
    #[test]
    fn merkaba_count() {
        assert_eq!(PatternKind::Merkaba.generate(0.0).len(), 7);
    }

    /// FlowerOfLife produces 13 hues.
    #[test]
    fn flower_of_life_count() {
        assert_eq!(PatternKind::FlowerOfLife.generate(0.0).len(), 13);
    }

    /// Metatron produces 14 hues.
    #[test]
    fn metatron_count() {
        assert_eq!(PatternKind::Metatron.generate(0.0).len(), 14);
    }

    /// SolfeggioAll produces 15 hues.
    #[test]
    fn solfeggio_count() {
        let hues = PatternKind::SolfeggioAll.generate(0.0);
        assert_eq!(hues.len(), 15);
    }

    /// Monochromatic produces exactly 1 hue.
    #[test]
    fn monochromatic_count() {
        assert_eq!(PatternKind::Monochromatic.generate(0.0).len(), 1);
    }

    /// generate_few truncates to at most 5.
    #[test]
    fn generate_few_truncates() {
        let hues = PatternKind::GoldenRatio.generate_few(0.0);
        assert_eq!(hues.len(), 5);
    }

    /// generate_few doesn't add hues.
    #[test]
    fn generate_few_monochromatic() {
        let hues = PatternKind::Monochromatic.generate_few(0.0);
        assert_eq!(hues.len(), 1);
    }

    /// Different base hues produce different results.
    #[test]
    fn different_bases() {
        let a = PatternKind::GoldenRatio.generate(0.0);
        let b = PatternKind::GoldenRatio.generate(90.0);
        assert_ne!(a, b);
    }

    /// Negative base hue wraps correctly.
    #[test]
    fn negative_base_wraps() {
        let hues = PatternKind::Triadic.generate(-30.0);
        for h in hues {
            assert!((0.0..360.0).contains(&h), "Hue out of range: {h}");
        }
    }

    /// Base hue > 360 wraps correctly.
    #[test]
    fn large_base_wraps() {
        let hues = PatternKind::Triadic.generate(720.0);
        for h in hues {
            assert!((0.0..360.0).contains(&h), "Hue out of range: {h}");
        }
    }

    /// SeedOfLife produces 19 hues.
    #[test]
    fn seed_of_life_count() {
        assert_eq!(PatternKind::SeedOfLife.generate(0.0).len(), 19);
    }

    /// HarmonicResonance produces 17 hues.
    #[test]
    fn harmonic_resonance_count() {
        assert_eq!(PatternKind::HarmonicResonance.generate(0.0).len(), 17);
    }

    /// PhiGrid produces 13 hues.
    #[test]
    fn phi_grid_count() {
        assert_eq!(PatternKind::PhiGrid.generate(0.0).len(), 13);
    }

    /// DivineProportion produces 10 hues.
    #[test]
    fn divine_proportion_count() {
        assert_eq!(PatternKind::DivineProportion.generate(0.0).len(), 10);
    }

    /// SriYantra produces 10 hues.
    #[test]
    fn sri_yantra_count() {
        assert_eq!(PatternKind::SriYantra.generate(0.0).len(), 10);
    }

    /// Torus produces 10 hues.
    #[test]
    fn torus_count() {
        assert_eq!(PatternKind::Torus.generate(0.0).len(), 10);
    }
}
