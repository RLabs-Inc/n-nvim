//! # n-theme — Sacred Geometry Mathematical Theming Engine
//!
//! Generates complete editor color themes from mathematical patterns.
//! One parameter shift (base hue, pattern kind, dark/light) produces an
//! entirely new harmonious theme with proper contrast, readability, and
//! visual hierarchy.
//!
//! # Architecture
//!
//! ```text
//! PatternKind + base_hue + is_dark + seed
//!     │
//!     ▼
//! pattern.rs:  generate hue array (pure math)
//!     │
//!     ▼
//! palette.rs:  assign hues to UI color roles (BG/FG/AC/diagnostics)
//!     │
//!     ▼
//! contrast.rs: enforce WCAG readability (>= 5.5:1 for text)
//!     │
//!     ▼
//! syntax.rs:   generate 30+ syntax token colors (grouped by family)
//!     │
//!     ▼
//! highlight.rs: assemble Theme with named HighlightGroups
//! ```
//!
//! # Color Space
//!
//! All generation happens in OKLCH (perceptually uniform). Colors are
//! gamut-mapped to sRGB and resolved to terminal-ready `CellColor` values
//! during theme construction. The hot rendering path never does color math.

// Single-char math variables are standard in color science.
#![allow(clippy::many_single_char_names)]
// Mathematical code uses small integer-to-float casts (loop indices, angles).
#![allow(clippy::cast_precision_loss)]
// Hue/lightness/chroma variable names are inherently similar.
#![allow(clippy::similar_names)]
// Pattern functions are inherently long — one match arm per pattern.
#![allow(clippy::too_many_lines)]
// f64→f32 truncation is intentional (PRNG values don't need f64 precision).
#![allow(clippy::cast_possible_truncation)]

pub mod builtin;
pub mod contrast;
pub mod highlight;
pub mod palette;
pub mod pattern;
pub mod syntax;

pub use highlight::{HighlightGroup, Theme};
pub use pattern::PatternKind;
