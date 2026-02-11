//! Vim-style modal editing.
//!
//! The editor is always in exactly one [`Mode`]. Each mode changes how input
//! is interpreted and how the cursor behaves:
//!
//! | Mode      | Cursor shape | Cursor limit          | Purpose              |
//! |-----------|--------------|----------------------|----------------------|
//! | Normal    | Block        | `0..content_len-1`   | Navigation, commands |
//! | Insert    | Bar          | `0..content_len`     | Typing text          |
//! | Visual    | Block        | `0..content_len-1`   | Selecting text       |
//! | Replace   | Underline    | `0..content_len-1`   | Overwriting text     |
//! | Command   | Bar          | (in command line)    | `:` commands         |

use std::fmt;

// ---------------------------------------------------------------------------
// VisualKind
// ---------------------------------------------------------------------------

/// The sub-mode of visual selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisualKind {
    /// `v` — character-wise selection.
    Char,
    /// `V` — line-wise selection (always selects full lines).
    Line,
    /// `Ctrl-V` — block (column) selection.
    Block,
}

impl fmt::Display for VisualKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char => f.write_str("VISUAL"),
            Self::Line => f.write_str("VISUAL LINE"),
            Self::Block => f.write_str("VISUAL BLOCK"),
        }
    }
}

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// The current editing mode.
///
/// This is a pure data type — it holds what mode we're in, not the logic
/// for handling keys. Key dispatch and mode transitions live in higher-level
/// code. The Mode enum just says "what are we doing right now."
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Default mode. Keys are commands, not text input.
    #[default]
    Normal,
    /// Text entry mode. Keys produce characters in the buffer.
    Insert,
    /// Selection mode. Movement extends the selection.
    Visual(VisualKind),
    /// Single-character replace mode (`r` in Vim replaces one char then
    /// returns to Normal — that's handled by the key layer, not here).
    /// This is `R` — continuous overwrite until Esc.
    Replace,
    /// Command-line mode (`:`, `/`, `?`). The cursor moves to the command
    /// line at the bottom of the screen.
    Command,
}

impl Mode {
    /// Human-readable name for the status line.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual(kind) => match kind {
                VisualKind::Char => "VISUAL",
                VisualKind::Line => "VISUAL LINE",
                VisualKind::Block => "VISUAL BLOCK",
            },
            Self::Replace => "REPLACE",
            Self::Command => "COMMAND",
        }
    }

    /// The terminal cursor shape for this mode.
    ///
    /// Returns a variant name matching `n_term::ansi::CursorShape`. We return
    /// a simple enum here rather than depending on n-term directly, keeping
    /// the editor core decoupled from the terminal backend.
    #[must_use]
    pub const fn cursor_shape(self) -> CursorShape {
        match self {
            Self::Normal | Self::Visual(_) => CursorShape::SteadyBlock,
            Self::Insert | Self::Command => CursorShape::SteadyBar,
            Self::Replace => CursorShape::SteadyUnderline,
        }
    }

    /// True if the cursor can sit one-past-the-last-char (insert/command).
    /// In normal, visual, and replace modes the cursor must be ON a character.
    #[inline]
    #[must_use]
    pub const fn cursor_past_end(self) -> bool {
        matches!(self, Self::Insert | Self::Command)
    }

    /// True if this mode accepts text input (insert, replace, command).
    #[inline]
    #[must_use]
    pub const fn is_input(self) -> bool {
        matches!(self, Self::Insert | Self::Replace | Self::Command)
    }

    /// True if we're in any visual sub-mode.
    #[inline]
    #[must_use]
    pub const fn is_visual(self) -> bool {
        matches!(self, Self::Visual(_))
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

// ---------------------------------------------------------------------------
// CursorShape (editor-local, mirrors n_term::ansi::CursorShape)
// ---------------------------------------------------------------------------

/// Cursor shape for terminal display.
///
/// This mirrors `n_term::ansi::CursorShape` so that n-editor doesn't depend
/// on n-term. The rendering layer maps this to the terminal-specific enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    /// `█` — solid block cursor.
    SteadyBlock,
    /// `▏` — thin vertical bar (line cursor).
    SteadyBar,
    /// `▁` — underline cursor.
    SteadyUnderline,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mode display names -------------------------------------------------

    #[test]
    fn mode_display_names() {
        assert_eq!(Mode::Normal.display_name(), "NORMAL");
        assert_eq!(Mode::Insert.display_name(), "INSERT");
        assert_eq!(Mode::Visual(VisualKind::Char).display_name(), "VISUAL");
        assert_eq!(Mode::Visual(VisualKind::Line).display_name(), "VISUAL LINE");
        assert_eq!(
            Mode::Visual(VisualKind::Block).display_name(),
            "VISUAL BLOCK"
        );
        assert_eq!(Mode::Replace.display_name(), "REPLACE");
        assert_eq!(Mode::Command.display_name(), "COMMAND");
    }

    #[test]
    fn mode_display_trait() {
        assert_eq!(format!("{}", Mode::Normal), "NORMAL");
        assert_eq!(format!("{}", Mode::Insert), "INSERT");
        assert_eq!(
            format!("{}", Mode::Visual(VisualKind::Block)),
            "VISUAL BLOCK"
        );
    }

    // -- Cursor shape -------------------------------------------------------

    #[test]
    fn cursor_shape_normal() {
        assert_eq!(Mode::Normal.cursor_shape(), CursorShape::SteadyBlock);
    }

    #[test]
    fn cursor_shape_insert() {
        assert_eq!(Mode::Insert.cursor_shape(), CursorShape::SteadyBar);
    }

    #[test]
    fn cursor_shape_visual() {
        assert_eq!(
            Mode::Visual(VisualKind::Char).cursor_shape(),
            CursorShape::SteadyBlock
        );
        assert_eq!(
            Mode::Visual(VisualKind::Line).cursor_shape(),
            CursorShape::SteadyBlock
        );
        assert_eq!(
            Mode::Visual(VisualKind::Block).cursor_shape(),
            CursorShape::SteadyBlock
        );
    }

    #[test]
    fn cursor_shape_replace() {
        assert_eq!(Mode::Replace.cursor_shape(), CursorShape::SteadyUnderline);
    }

    #[test]
    fn cursor_shape_command() {
        assert_eq!(Mode::Command.cursor_shape(), CursorShape::SteadyBar);
    }

    // -- cursor_past_end ----------------------------------------------------

    #[test]
    fn cursor_past_end_insert_and_command() {
        assert!(Mode::Insert.cursor_past_end());
        assert!(Mode::Command.cursor_past_end());
    }

    #[test]
    fn cursor_past_end_false_for_others() {
        assert!(!Mode::Normal.cursor_past_end());
        assert!(!Mode::Visual(VisualKind::Char).cursor_past_end());
        assert!(!Mode::Replace.cursor_past_end());
    }

    // -- is_input -----------------------------------------------------------

    #[test]
    fn is_input_true_for_typing_modes() {
        assert!(Mode::Insert.is_input());
        assert!(Mode::Replace.is_input());
        assert!(Mode::Command.is_input());
    }

    #[test]
    fn is_input_false_for_command_modes() {
        assert!(!Mode::Normal.is_input());
        assert!(!Mode::Visual(VisualKind::Char).is_input());
    }

    // -- is_visual ----------------------------------------------------------

    #[test]
    fn is_visual() {
        assert!(Mode::Visual(VisualKind::Char).is_visual());
        assert!(Mode::Visual(VisualKind::Line).is_visual());
        assert!(Mode::Visual(VisualKind::Block).is_visual());
        assert!(!Mode::Normal.is_visual());
        assert!(!Mode::Insert.is_visual());
    }

    // -- Default ------------------------------------------------------------

    #[test]
    fn default_is_normal() {
        assert_eq!(Mode::default(), Mode::Normal);
    }

    // -- Equality & Debug ---------------------------------------------------

    #[test]
    fn mode_equality() {
        assert_eq!(Mode::Normal, Mode::Normal);
        assert_ne!(Mode::Normal, Mode::Insert);
        assert_eq!(
            Mode::Visual(VisualKind::Char),
            Mode::Visual(VisualKind::Char)
        );
        assert_ne!(
            Mode::Visual(VisualKind::Char),
            Mode::Visual(VisualKind::Line)
        );
    }

    #[test]
    fn mode_debug() {
        let debug = format!("{:?}", Mode::Visual(VisualKind::Block));
        assert!(debug.contains("Visual"));
        assert!(debug.contains("Block"));
    }

    // -- VisualKind display -------------------------------------------------

    #[test]
    fn visual_kind_display() {
        assert_eq!(format!("{}", VisualKind::Char), "VISUAL");
        assert_eq!(format!("{}", VisualKind::Line), "VISUAL LINE");
        assert_eq!(format!("{}", VisualKind::Block), "VISUAL BLOCK");
    }
}
