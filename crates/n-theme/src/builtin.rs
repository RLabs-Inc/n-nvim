//! Named preset themes — ready-to-use configurations.
//!
//! Each preset is a specific combination of pattern, base hue, dark/light,
//! and seed that produces a distinctive, coherent theme.

use crate::highlight::Theme;
use crate::pattern::PatternKind;

/// Look up a builtin theme by name.
///
/// Returns `None` if the name is not recognized.
#[must_use]
pub fn builtin_theme(name: &str) -> Option<Theme> {
    Some(match name {
        "terminal" => return Some(Theme::terminal()),
        "default" | "golden-dark" => {
            Theme::generate("golden-dark", PatternKind::GoldenRatio, 270.0, true, false, 42)
        }
        "golden-light" => {
            Theme::generate("golden-light", PatternKind::GoldenRatio, 270.0, false, false, 42)
        }
        "fibonacci" => {
            Theme::generate("fibonacci", PatternKind::Fibonacci, 220.0, true, false, 37)
        }
        "merkaba" => {
            Theme::generate("merkaba", PatternKind::Merkaba, 280.0, true, false, 55)
        }
        "solfeggio" => {
            Theme::generate("solfeggio", PatternKind::SolfeggioAll, 260.0, true, false, 63)
        }
        "monochrome" => {
            Theme::generate("monochrome", PatternKind::Monochromatic, 270.0, true, true, 42)
        }
        "triadic" => {
            Theme::generate("triadic", PatternKind::Triadic, 240.0, true, false, 48)
        }
        "pentagram" => {
            Theme::generate("pentagram", PatternKind::Pentagram, 300.0, true, false, 71)
        }
        _ => return None,
    })
}

/// List all available builtin theme names.
#[must_use]
pub const fn builtin_names() -> &'static [&'static str] {
    &[
        "terminal",
        "default",
        "golden-dark",
        "golden-light",
        "fibonacci",
        "merkaba",
        "solfeggio",
        "monochrome",
        "triadic",
        "pentagram",
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_resolve() {
        for name in builtin_names() {
            let theme = builtin_theme(name);
            assert!(theme.is_some(), "Builtin '{name}' failed to generate");
        }
    }

    #[test]
    fn unknown_returns_none() {
        assert!(builtin_theme("nonexistent").is_none());
    }

    #[test]
    fn default_is_golden_dark() {
        let a = builtin_theme("default").unwrap();
        let b = builtin_theme("golden-dark").unwrap();
        assert_eq!(a.name, b.name);
        assert_eq!(a.normal, b.normal);
    }

    #[test]
    fn golden_light_is_light() {
        let t = builtin_theme("golden-light").unwrap();
        assert!(!t.is_dark);
    }

    #[test]
    fn fibonacci_has_correct_name() {
        let t = builtin_theme("fibonacci").unwrap();
        assert_eq!(t.name, "fibonacci");
    }

    #[test]
    fn each_builtin_is_distinct() {
        let default = builtin_theme("default").unwrap();
        let fibonacci = builtin_theme("fibonacci").unwrap();
        let merkaba = builtin_theme("merkaba").unwrap();
        // Different themes should produce different accent colors.
        assert_ne!(default.status_line.bg, fibonacci.status_line.bg);
        assert_ne!(fibonacci.status_line.bg, merkaba.status_line.bg);
    }
}
