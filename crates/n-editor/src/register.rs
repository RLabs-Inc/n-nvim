//! Register — storage for yanked and deleted text.
//!
//! In Vim, every yank (`y`) and delete (`d`, `x`, `dd`) copies text into the
//! unnamed register. Paste (`p`, `P`) retrieves it. The register tracks whether
//! the text was captured character-wise or line-wise, because paste behaves
//! differently for each:
//!
//! - **Char-wise**: `p` inserts after cursor, `P` inserts before cursor.
//! - **Line-wise**: `p` inserts on a new line below, `P` above.
//!
//! This module implements only the unnamed register (`""`). Named registers
//! (`"a`–`"z`) and the system clipboard (`"+`) are future work.

/// How the register content was captured — determines paste behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterKind {
    /// Character-wise (from `v` visual or single-char delete).
    /// Paste inserts inline at cursor position.
    Char,

    /// Line-wise (from `V` visual or `dd`).
    /// Paste inserts entire lines above or below the cursor line.
    Line,
}

/// The unnamed register — holds the most recent yank or delete.
#[derive(Debug, Clone)]
pub struct Register {
    /// The stored text. Empty string when nothing has been yanked yet.
    content: String,

    /// How the text was captured. Defaults to `Char` when empty (doesn't
    /// matter since paste is a no-op on empty content).
    kind: RegisterKind,
}

impl Register {
    /// Create an empty register.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            content: String::new(),
            kind: RegisterKind::Char,
        }
    }

    /// Store text in the register, replacing any previous content.
    pub fn yank(&mut self, text: String, kind: RegisterKind) {
        self.content = text;
        self.kind = kind;
    }

    /// The stored text. Empty if nothing has been yanked.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// How the text was captured — determines paste behavior.
    #[must_use]
    pub const fn kind(&self) -> RegisterKind {
        self.kind
    }

    /// True if the register has no content (nothing to paste).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

impl Default for Register {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ───────────────────────────────────────────────────

    #[test]
    fn new_register_is_empty() {
        let reg = Register::new();
        assert!(reg.is_empty());
        assert_eq!(reg.content(), "");
    }

    #[test]
    fn default_register_is_empty() {
        let reg = Register::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn new_register_defaults_to_char_kind() {
        let reg = Register::new();
        assert_eq!(reg.kind(), RegisterKind::Char);
    }

    // ── Yank and retrieve ──────────────────────────────────────────────

    #[test]
    fn yank_char_stores_content() {
        let mut reg = Register::new();
        reg.yank("hello".into(), RegisterKind::Char);
        assert_eq!(reg.content(), "hello");
        assert_eq!(reg.kind(), RegisterKind::Char);
        assert!(!reg.is_empty());
    }

    #[test]
    fn yank_line_stores_content() {
        let mut reg = Register::new();
        reg.yank("entire line\n".into(), RegisterKind::Line);
        assert_eq!(reg.content(), "entire line\n");
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    #[test]
    fn yank_replaces_previous_content() {
        let mut reg = Register::new();
        reg.yank("first".into(), RegisterKind::Char);
        reg.yank("second".into(), RegisterKind::Line);
        assert_eq!(reg.content(), "second");
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    #[test]
    fn yank_replaces_kind() {
        let mut reg = Register::new();
        reg.yank("text".into(), RegisterKind::Line);
        assert_eq!(reg.kind(), RegisterKind::Line);
        reg.yank("text".into(), RegisterKind::Char);
        assert_eq!(reg.kind(), RegisterKind::Char);
    }

    #[test]
    fn yank_empty_string() {
        let mut reg = Register::new();
        reg.yank("something".into(), RegisterKind::Char);
        reg.yank(String::new(), RegisterKind::Char);
        assert!(reg.is_empty());
        assert_eq!(reg.content(), "");
    }

    #[test]
    fn yank_multiline_char() {
        let mut reg = Register::new();
        reg.yank("line one\nline two\nline three".into(), RegisterKind::Char);
        assert_eq!(reg.content(), "line one\nline two\nline three");
        assert_eq!(reg.kind(), RegisterKind::Char);
    }

    #[test]
    fn yank_multiline_line() {
        let mut reg = Register::new();
        let text = "fn main() {\n    println!(\"hi\");\n}\n";
        reg.yank(text.into(), RegisterKind::Line);
        assert_eq!(reg.content(), text);
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    // ── Clone ──────────────────────────────────────────────────────────

    #[test]
    fn clone_preserves_content_and_kind() {
        let mut reg = Register::new();
        reg.yank("cloned".into(), RegisterKind::Line);
        let copy = reg.clone();
        assert_eq!(copy.content(), "cloned");
        assert_eq!(copy.kind(), RegisterKind::Line);
    }

    // ── RegisterKind ───────────────────────────────────────────────────

    #[test]
    fn register_kind_equality() {
        assert_eq!(RegisterKind::Char, RegisterKind::Char);
        assert_eq!(RegisterKind::Line, RegisterKind::Line);
        assert_ne!(RegisterKind::Char, RegisterKind::Line);
    }

    #[test]
    fn register_kind_copy() {
        let kind = RegisterKind::Line;
        let copy = kind;
        assert_eq!(kind, copy);
    }

    #[test]
    fn register_kind_debug() {
        assert_eq!(format!("{:?}", RegisterKind::Char), "Char");
        assert_eq!(format!("{:?}", RegisterKind::Line), "Line");
    }

    // ── Unicode ────────────────────────────────────────────────────────

    #[test]
    fn yank_unicode_content() {
        let mut reg = Register::new();
        reg.yank("日本語テスト".into(), RegisterKind::Char);
        assert_eq!(reg.content(), "日本語テスト");
    }

    #[test]
    fn yank_emoji_content() {
        let mut reg = Register::new();
        reg.yank("hello 🎉🚀 world".into(), RegisterKind::Char);
        assert_eq!(reg.content(), "hello 🎉🚀 world");
    }
}
