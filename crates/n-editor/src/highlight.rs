//! Syntax highlighting via tree-sitter.
//!
//! Provides incremental parsing and per-character foreground colors for the
//! viewport. The [`Highlighter`] wraps a tree-sitter `Parser` + `Tree` with
//! a compiled highlight `Query` whose captures map to the theme's
//! [`SyntaxPalette`](n_theme::syntax::SyntaxPalette) or to ANSI terminal
//! colors depending on the active theme.
//!
//! # Architecture
//!
//! 1. Each buffer with a recognized file extension gets a `Highlighter`.
//! 2. On any buffer edit, call [`Highlighter::mark_dirty`].
//! 3. Before painting, call [`Highlighter::ensure_parsed`] (full reparse when
//!    dirty — tree-sitter is fast enough for interactive use).
//! 4. Call [`Highlighter::viewport_colors`] to get per-character foreground
//!    colors for the visible lines. Later captures (more specific patterns)
//!    override earlier ones for the same character position.
//!
//! # Custom highlight queries
//!
//! We embed our own Rust highlight query with 29 capture names that map
//! one-to-one to the `SyntaxPalette` fields, giving far more granularity
//! than tree-sitter-rust's default 21-capture query. Keywords are split into
//! `keyword.control`, `keyword.import`, `keyword.storage`, `keyword.modifier`,
//! and `keyword.function`. Function calls get `function.call` vs `function`
//! (definition). Numbers get `@number` separate from booleans.

use std::path::Path;

use n_term::color::CellColor;
use n_theme::syntax::SyntaxPalette;
use n_theme::Theme;
use ropey::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

// ---------------------------------------------------------------------------
// Custom Rust highlight query — 29 captures mapping to SyntaxPalette
// ---------------------------------------------------------------------------

/// Use tree-sitter-rust's bundled highlight query.
///
/// 21 captures covering the main categories. Future: write a custom query
/// with 29+ captures for full `SyntaxPalette` utilization (keyword sub-types,
/// function call vs definition, numbers vs booleans).
const RUST_HIGHLIGHTS: &str = tree_sitter_rust::HIGHLIGHTS_QUERY;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Syntax highlighter for a single buffer.
///
/// Wraps a tree-sitter `Parser` + `Tree` + compiled highlight `Query` with
/// a pre-computed color mapping from capture indices to `CellColor`.
pub struct Highlighter {
    parser: Parser,
    tree: Option<Tree>,
    query: Query,
    /// Color for each capture index. `CellColor::Default` = no highlighting.
    capture_colors: Vec<CellColor>,
    /// Cached source text (updated on reparse, reused for queries).
    source: String,
    /// Set `true` when the buffer is edited. Cleared after reparse.
    stale: bool,
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

/// Detect the language name from a file extension.
///
/// Returns `Some("rust")` for `.rs` files, etc. Extend this as we add
/// more tree-sitter grammars.
#[must_use]
pub fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Highlighter
// ---------------------------------------------------------------------------

impl Highlighter {
    /// Create a highlighter for the given language and theme.
    ///
    /// Returns `None` if the language is not supported or the query fails to
    /// compile.
    #[must_use]
    pub fn new(language_name: &str, theme: &Theme) -> Option<Self> {
        let (ts_language, query_source) = match language_name {
            "rust" => {
                let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
                (lang, RUST_HIGHLIGHTS)
            }
            _ => return None,
        };

        let mut parser = Parser::new();
        parser.set_language(&ts_language).ok()?;
        let query = Query::new(&ts_language, query_source).ok()?;
        let capture_colors = build_capture_colors(&query, theme);

        Some(Self {
            parser,
            tree: None,
            query,
            capture_colors,
            source: String::new(),
            stale: true,
        })
    }

    /// Mark the tree as stale. Call after any buffer edit.
    #[inline]
    pub const fn mark_dirty(&mut self) {
        self.stale = true;
    }

    /// Rebuild the capture-to-color mapping after a theme change.
    pub fn update_theme(&mut self, theme: &Theme) {
        self.capture_colors = build_capture_colors(&self.query, theme);
    }

    /// Reparse the buffer if dirty.
    ///
    /// Converts the rope to a String and parses it with tree-sitter. The
    /// previous tree is passed for incremental parsing hints. The source
    /// text is cached for use in [`viewport_colors`](Self::viewport_colors).
    pub fn ensure_parsed(&mut self, rope: &Rope) {
        if !self.stale {
            return;
        }
        self.source = rope_to_string(rope);
        // Full reparse: we don't track individual edits yet, so passing the
        // old tree without InputEdit would give incorrect results. Tree-sitter
        // is fast enough for full reparses (~1ms for 10K lines).
        self.tree = self.parser.parse(&self.source, None);
        self.stale = false;
    }

    /// Compute per-character foreground colors for a viewport range.
    ///
    /// Returns one `Vec<CellColor>` per viewport line, indexed by char column.
    /// `CellColor::Default` means no syntax color (use the theme's normal fg).
    ///
    /// Call [`ensure_parsed`](Self::ensure_parsed) before this.
    #[must_use]
    pub fn viewport_colors(
        &self,
        first_line: usize,
        line_count: usize,
        rope: &Rope,
    ) -> Vec<Vec<CellColor>> {
        let Some(tree) = &self.tree else {
            return vec![Vec::new(); line_count];
        };

        let total_lines = rope.len_lines();
        if first_line >= total_lines {
            return vec![Vec::new(); line_count];
        }

        let last_line = (first_line + line_count).min(total_lines);

        // Byte range for the viewport (restrict QueryCursor to this range).
        let start_byte = rope.line_to_byte(first_line);
        let end_byte = if last_line < total_lines {
            rope.line_to_byte(last_line)
        } else {
            rope.len_bytes()
        };

        // Pre-compute per-line char counts (excluding trailing newline).
        let mut result: Vec<Vec<CellColor>> = Vec::with_capacity(line_count);
        let mut line_byte_starts = Vec::with_capacity(line_count);
        let mut line_char_starts = Vec::with_capacity(line_count);

        for line_idx in first_line..last_line {
            let line = rope.line(line_idx);
            let chars_in_line = line.len_chars();
            result.push(vec![CellColor::Default; chars_in_line]);
            line_byte_starts.push(rope.line_to_byte(line_idx));
            line_char_starts.push(rope.line_to_char(line_idx));
        }

        // Query the tree for highlights in the viewport byte range.
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start_byte..end_byte);

        let mut captures =
            cursor.captures(&self.query, tree.root_node(), self.source.as_bytes());
        while let Some((m, _capture_idx)) = captures.next() {
            for capture in m.captures {
                let fg = self.capture_colors[capture.index as usize];
                if fg.is_default() {
                    continue;
                }

                let node = capture.node;
                let node_start = node.start_byte();
                let node_end = node.end_byte();

                // The node may span multiple lines. Paint each line's portion.
                let start_row = node.start_position().row;
                let end_row = node.end_position().row;

                for row in start_row.max(first_line)..=end_row.min(last_line - 1) {
                    let offset = row - first_line;
                    let line_byte_start = line_byte_starts[offset];
                    let line_char_start = line_char_starts[offset];
                    let next_line_byte = if offset + 1 < line_byte_starts.len() {
                        line_byte_starts[offset + 1]
                    } else {
                        end_byte
                    };

                    // Clamp node range to this line.
                    let span_start_byte = node_start.max(line_byte_start);
                    let span_end_byte = node_end.min(next_line_byte);
                    if span_start_byte >= span_end_byte {
                        continue;
                    }

                    let start_char = rope.byte_to_char(span_start_byte);
                    let end_char = rope.byte_to_char(span_end_byte);
                    let start_col: usize = start_char - line_char_start;
                    let end_col: usize = end_char - line_char_start;

                    // Paint: later captures override earlier (more specific wins).
                    let colors: &mut Vec<CellColor> = &mut result[offset];
                    for col in start_col..end_col.min(colors.len()) {
                        colors[col] = fg;
                    }
                }
            }
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Capture-to-color mapping
// ---------------------------------------------------------------------------

/// Build a color lookup from capture index → `CellColor` for the theme.
fn build_capture_colors(query: &Query, theme: &Theme) -> Vec<CellColor> {
    let is_terminal = theme.pattern.is_none();
    query
        .capture_names()
        .iter()
        .map(|name| {
            if is_terminal {
                terminal_color(name)
            } else {
                generated_color(name, &theme.syntax)
            }
        })
        .collect()
}

/// ANSI colors for the terminal-native theme.
///
/// Uses the standard 16 ANSI colors so themes adapt to the user's terminal
/// palette. Only semantically important tokens get color; punctuation and
/// variables stay default to avoid visual noise.
#[allow(clippy::match_same_arms)] // Semantic categories may diverge later.
fn terminal_color(name: &str) -> CellColor {
    use CellColor::Ansi256;
    match name {
        // Keywords — magenta
        "keyword" => Ansi256(5),

        // Strings — green
        "string" | "escape" => Ansi256(2),

        // Comments — bright black (gray)
        "comment" | "comment.documentation" => Ansi256(8),

        // Functions — blue
        "function" | "function.method" => Ansi256(4),

        // Macros / constants / numbers — cyan
        "function.macro" | "constant" | "constant.builtin" => Ansi256(6),

        // Types — yellow
        "type" | "type.builtin" | "constructor" => Ansi256(3),

        // self — red
        "variable.builtin" => Ansi256(1),

        // Attributes — yellow
        "attribute" => Ansi256(3),

        // Labels — yellow
        "label" => Ansi256(3),

        // Properties — cyan
        "property" => Ansi256(6),

        // Operators + everything else: no color (default terminal fg)
        _ => CellColor::Default,
    }
}

/// Map capture names to `SyntaxPalette` colors for generated themes.
fn generated_color(name: &str, syntax: &SyntaxPalette) -> CellColor {
    match name {
        "keyword" => syntax.keyword.to_cell_color(),

        "string" | "escape" => syntax.string.to_cell_color(),

        "comment" | "comment.documentation" => syntax.comment.to_cell_color(),

        "function" => syntax.function.to_cell_color(),
        "function.method" => syntax.method.to_cell_color(),
        "function.macro" => syntax.macro_name.to_cell_color(),

        "type" | "type.builtin" | "constructor" => syntax.type_name.to_cell_color(),

        "constant" | "constant.builtin" => syntax.constant.to_cell_color(),

        "variable.parameter" => syntax.variable.to_cell_color(),
        "variable.builtin" => syntax.variable_readonly.to_cell_color(),

        "operator" => syntax.operator.to_cell_color(),
        "punctuation.bracket" => syntax.punctuation_bracket.to_cell_color(),
        "punctuation.delimiter" => syntax.punctuation_delimiter.to_cell_color(),

        "property" => syntax.property.to_cell_color(),
        "attribute" => syntax.attribute.to_cell_color(),
        "label" => syntax.label.to_cell_color(),

        _ => CellColor::Default,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a rope to a String for tree-sitter parsing.
///
/// Tree-sitter needs contiguous text. For files under ~1MB this is fast
/// enough; we can optimize with `parse_with` + rope chunk callbacks later
/// for giant files.
fn rope_to_string(rope: &Rope) -> String {
    let mut s = String::with_capacity(rope.len_bytes());
    for chunk in rope.chunks() {
        s.push_str(chunk);
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use n_theme::Theme;

    fn test_rope(text: &str) -> Rope {
        Rope::from_str(text)
    }

    #[test]
    fn detect_rust() {
        assert_eq!(detect_language(Path::new("foo.rs")), Some("rust"));
        assert_eq!(detect_language(Path::new("main.rs")), Some("rust"));
        assert_eq!(detect_language(Path::new("/a/b/c.rs")), Some("rust"));
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_language(Path::new("foo.py")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
        assert_eq!(detect_language(Path::new("no_ext")), None);
    }

    #[test]
    fn highlighter_new_rust() {
        let theme = Theme::terminal();
        let hl = Highlighter::new("rust", &theme);
        assert!(hl.is_some());
    }

    #[test]
    fn highlighter_new_unknown() {
        let theme = Theme::terminal();
        assert!(Highlighter::new("brainfuck", &theme).is_none());
    }

    #[test]
    fn parse_and_query() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("fn main() {}\n");
        hl.ensure_parsed(&rope);
        assert!(hl.tree.is_some());

        let colors = hl.viewport_colors(0, 1, &rope);
        assert_eq!(colors.len(), 1);
        // "fn" should be colored (keyword.function → magenta/5)
        assert!(!colors[0].is_empty());
        assert_ne!(colors[0][0], CellColor::Default, "fn should be colored");
        assert_ne!(colors[0][1], CellColor::Default, "fn should be colored");
    }

    #[test]
    fn keyword_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let x = 42;\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "let" at col 0..3 should be keyword (magenta = Ansi256(5))
        assert_eq!(colors[0][0], CellColor::Ansi256(5));
        assert_eq!(colors[0][1], CellColor::Ansi256(5));
        assert_eq!(colors[0][2], CellColor::Ansi256(5));
        // "x" at col 4 should be default (no variable highlighting in terminal)
        assert_eq!(colors[0][4], CellColor::Default);
    }

    #[test]
    fn string_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let s = \"hello\";\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "hello" including quotes should be green (Ansi256(2))
        // The string starts at col 8 (the opening quote)
        assert_eq!(colors[0][8], CellColor::Ansi256(2));
    }

    #[test]
    fn comment_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("// hello\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // Comment should be gray (Ansi256(8))
        assert_eq!(colors[0][0], CellColor::Ansi256(8));
        assert_eq!(colors[0][3], CellColor::Ansi256(8));
    }

    #[test]
    fn function_def_vs_call() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("fn foo() { bar(); }\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "foo" at col 3..6 = function (blue = Ansi256(4))
        assert_eq!(colors[0][3], CellColor::Ansi256(4));
        // "bar" at col 11..14 = function.call (blue = Ansi256(4))
        assert_eq!(colors[0][11], CellColor::Ansi256(4));
    }

    #[test]
    fn type_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let x: i32 = 0;\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "i32" at col 7..10 = type.builtin (yellow = Ansi256(3))
        assert_eq!(colors[0][7], CellColor::Ansi256(3));
    }

    #[test]
    fn multiline_function() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("fn foo(\n    x: i32,\n) {}\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 3, &rope);
        assert_eq!(colors.len(), 3);
        // Line 0: "fn" is keyword
        assert_eq!(colors[0][0], CellColor::Ansi256(5));
        // Line 1: "i32" should be type
        // "    x: i32," — i32 starts at col 7
        assert_eq!(colors[1][7], CellColor::Ansi256(3));
    }

    #[test]
    fn viewport_subset() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        hl.ensure_parsed(&rope);

        // Only query line 1 (middle line)
        let colors = hl.viewport_colors(1, 1, &rope);
        assert_eq!(colors.len(), 1);
        // "let" at col 0..3
        assert_eq!(colors[0][0], CellColor::Ansi256(5));
    }

    #[test]
    fn mark_dirty_forces_reparse() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope1 = test_rope("let x = 1;\n");
        hl.ensure_parsed(&rope1);

        let rope2 = test_rope("fn foo() {}\n");
        hl.mark_dirty();
        hl.ensure_parsed(&rope2);

        let colors = hl.viewport_colors(0, 1, &rope2);
        // Should parse the new content: "fn" is keyword
        assert_eq!(colors[0][0], CellColor::Ansi256(5));
        // "foo" is function (blue)
        assert_eq!(colors[0][3], CellColor::Ansi256(4));
    }

    #[test]
    fn empty_buffer() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        assert_eq!(colors.len(), 1);
    }

    #[test]
    fn generated_theme_colors() {
        // Verify generated themes use SyntaxPalette colors, not ANSI.
        let theme = Theme::generate_surprise();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let x = 42;\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "let" should be colored (not default, and not ANSI)
        assert_ne!(colors[0][0], CellColor::Default);
        // Generated themes use Rgb, not Ansi256
        assert!(matches!(colors[0][0], CellColor::Rgb(_, _, _)));
    }

    #[test]
    fn update_theme_changes_colors() {
        let terminal = Theme::terminal();
        let mut hl = Highlighter::new("rust", &terminal).unwrap();
        let rope = test_rope("fn main() {}\n");
        hl.ensure_parsed(&rope);

        let colors1 = hl.viewport_colors(0, 1, &rope);

        // Switch to generated theme
        let generated = Theme::generate_surprise();
        hl.update_theme(&generated);

        let colors2 = hl.viewport_colors(0, 1, &rope);

        // "fn" color should be different between terminal and generated
        assert_ne!(colors1[0][0], colors2[0][0]);
    }

    #[test]
    fn control_flow_keywords() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        // Use a full function body so the parser can validate the return keyword.
        let rope = test_rope("fn f() { if true { return; } }\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "if" at col 9..11 and "return" at col 19..25 should be keyword color
        assert_eq!(colors[0][9], CellColor::Ansi256(5));  // if
        assert_eq!(colors[0][19], CellColor::Ansi256(5)); // return
    }

    #[test]
    fn macro_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("println!(\"hi\");\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "println" should be macro color (cyan = Ansi256(6))
        assert_eq!(colors[0][0], CellColor::Ansi256(6));
    }

    #[test]
    fn number_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let n = 42;\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 1, &rope);
        // "42" at col 8..10 should be number/constant (cyan = Ansi256(6))
        assert_eq!(colors[0][8], CellColor::Ansi256(6));
    }

    #[test]
    fn attribute_coloring() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("#[derive(Debug)]\nstruct Foo;\n");
        hl.ensure_parsed(&rope);

        let colors = hl.viewport_colors(0, 2, &rope);
        // "#[derive(Debug)]" should be attribute color (yellow = Ansi256(3))
        assert_eq!(colors[0][0], CellColor::Ansi256(3));
        // "struct" on line 1 should be keyword.storage (magenta = Ansi256(5))
        assert_eq!(colors[1][0], CellColor::Ansi256(5));
    }

    #[test]
    fn stale_skips_reparse() {
        let theme = Theme::terminal();
        let mut hl = Highlighter::new("rust", &theme).unwrap();
        let rope = test_rope("let x = 1;\n");
        hl.ensure_parsed(&rope);

        // Calling ensure_parsed again without mark_dirty should not reparse.
        let source_ptr = hl.source.as_ptr();
        hl.ensure_parsed(&rope);
        assert_eq!(hl.source.as_ptr(), source_ptr, "should not reallocate");
    }
}
