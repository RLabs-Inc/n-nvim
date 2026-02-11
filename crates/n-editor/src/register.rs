//! Register — storage for yanked and deleted text.
//!
//! Vim registers are the clipboard system. Every yank (`y`) and delete (`d`,
//! `x`, `dd`) copies text into a register. Paste (`p`, `P`) retrieves it.
//!
//! The register tracks whether text was captured character-wise or line-wise,
//! because paste behaves differently for each:
//!
//! - **Char-wise**: `p` inserts after cursor, `P` inserts before cursor.
//! - **Line-wise**: `p` inserts on a new line below, `P` above.
//!
//! ## Register types
//!
//! - **Unnamed (`""`)**: The default register. All yank/delete operations
//!   write here automatically, even when targeting a named register.
//! - **Named (`"a`–`"z`)**: 26 user-selectable registers. Lowercase
//!   overwrites, uppercase (`"A`–`"Z`) appends to the corresponding
//!   lowercase register.

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

/// A single register slot — holds text and its capture kind.
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

    /// Append text to the register (for uppercase register names).
    ///
    /// If either the existing content or the appended text is line-wise,
    /// the register becomes line-wise and a newline separator is inserted.
    pub fn append(&mut self, text: &str, kind: RegisterKind) {
        if kind == RegisterKind::Line || self.kind == RegisterKind::Line {
            // Ensure existing content ends with newline before appending.
            if !self.content.is_empty() && !self.content.ends_with('\n') {
                self.content.push('\n');
            }
            self.content.push_str(text);
            self.kind = RegisterKind::Line;
        } else {
            self.content.push_str(text);
        }
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

// ── Register file ────────────────────────────────────────────────────────

/// A complete register file — unnamed register + 26 named registers (a-z).
///
/// All yank/delete operations write to the unnamed register automatically.
/// When a named register is specified via `"a`, the text goes to both the
/// named register and the unnamed register. Uppercase names (`"A`) append
/// to the named register instead of overwriting.
pub struct RegisterFile {
    /// The unnamed register — receives every yank and delete.
    unnamed: Register,

    /// Named registers a-z. Indexed by `ch as u8 - b'a'`.
    named: [Register; 26],
}

impl RegisterFile {
    /// Create a register file with all registers empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            unnamed: Register::new(),
            named: std::array::from_fn(|_| Register::new()),
        }
    }

    /// Store text in a register.
    ///
    /// - `name == None` → unnamed only (default behavior)
    /// - `name == Some('a'..='z')` → overwrite named, copy to unnamed
    /// - `name == Some('A'..='Z')` → append to named, copy result to unnamed
    ///
    /// Any other name falls back to unnamed-only.
    pub fn yank(&mut self, name: Option<char>, text: String, kind: RegisterKind) {
        match name {
            Some(ch @ 'a'..='z') => {
                let idx = (ch as u8 - b'a') as usize;
                self.named[idx].yank(text.clone(), kind);
                self.unnamed.yank(text, kind);
            }
            Some(ch @ 'A'..='Z') => {
                let idx = (ch as u8 - b'A') as usize;
                self.named[idx].append(&text, kind);
                // Unnamed gets the full (appended) content.
                let full = self.named[idx].content().to_string();
                let full_kind = self.named[idx].kind();
                self.unnamed.yank(full, full_kind);
            }
            // None or unrecognized name → unnamed only.
            _ => {
                self.unnamed.yank(text, kind);
            }
        }
    }

    /// Get the register to read from.
    ///
    /// - `None` → unnamed register
    /// - `Some('a'..='z')` → named register
    /// - `Some('A'..='Z')` → same as lowercase (reads are case-insensitive)
    ///
    /// Any other name falls back to unnamed.
    #[must_use]
    pub const fn get(&self, name: Option<char>) -> &Register {
        match name {
            Some(ch) if ch.is_ascii_lowercase() => {
                &self.named[(ch as u8 - b'a') as usize]
            }
            Some(ch) if ch.is_ascii_uppercase() => {
                &self.named[(ch as u8 - b'A') as usize]
            }
            // None or unrecognized → unnamed.
            _ => &self.unnamed,
        }
    }
}

impl Default for RegisterFile {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Register (individual slot) ──────────────────────────────────────

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

    #[test]
    fn clone_preserves_content_and_kind() {
        let mut reg = Register::new();
        reg.yank("cloned".into(), RegisterKind::Line);
        let copy = reg.clone();
        assert_eq!(copy.content(), "cloned");
        assert_eq!(copy.kind(), RegisterKind::Line);
    }

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

    // ── Append ──────────────────────────────────────────────────────────

    #[test]
    fn append_char_to_empty() {
        let mut reg = Register::new();
        reg.append("hello", RegisterKind::Char);
        assert_eq!(reg.content(), "hello");
        assert_eq!(reg.kind(), RegisterKind::Char);
    }

    #[test]
    fn append_char_to_char() {
        let mut reg = Register::new();
        reg.yank("foo".into(), RegisterKind::Char);
        reg.append("bar", RegisterKind::Char);
        assert_eq!(reg.content(), "foobar");
        assert_eq!(reg.kind(), RegisterKind::Char);
    }

    #[test]
    fn append_line_to_char_upgrades_kind() {
        let mut reg = Register::new();
        reg.yank("first".into(), RegisterKind::Char);
        reg.append("second\n", RegisterKind::Line);
        assert_eq!(reg.content(), "first\nsecond\n");
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    #[test]
    fn append_char_to_line_stays_line() {
        let mut reg = Register::new();
        reg.yank("first\n".into(), RegisterKind::Line);
        reg.append("second", RegisterKind::Char);
        assert_eq!(reg.content(), "first\nsecond");
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    #[test]
    fn append_line_to_line() {
        let mut reg = Register::new();
        reg.yank("line one\n".into(), RegisterKind::Line);
        reg.append("line two\n", RegisterKind::Line);
        assert_eq!(reg.content(), "line one\nline two\n");
        assert_eq!(reg.kind(), RegisterKind::Line);
    }

    // ── RegisterFile ────────────────────────────────────────────────────

    #[test]
    fn register_file_new_all_empty() {
        let rf = RegisterFile::new();
        assert!(rf.get(None).is_empty());
        for ch in 'a'..='z' {
            assert!(rf.get(Some(ch)).is_empty());
        }
    }

    #[test]
    fn register_file_default() {
        let rf = RegisterFile::default();
        assert!(rf.get(None).is_empty());
    }

    #[test]
    fn yank_unnamed() {
        let mut rf = RegisterFile::new();
        rf.yank(None, "hello".into(), RegisterKind::Char);
        assert_eq!(rf.get(None).content(), "hello");
        assert_eq!(rf.get(None).kind(), RegisterKind::Char);
    }

    #[test]
    fn yank_named_writes_both() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "world".into(), RegisterKind::Line);
        // Named register gets it.
        assert_eq!(rf.get(Some('a')).content(), "world");
        assert_eq!(rf.get(Some('a')).kind(), RegisterKind::Line);
        // Unnamed register also gets it.
        assert_eq!(rf.get(None).content(), "world");
        assert_eq!(rf.get(None).kind(), RegisterKind::Line);
    }

    #[test]
    fn yank_named_isolates_registers() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "alpha".into(), RegisterKind::Char);
        rf.yank(Some('b'), "bravo".into(), RegisterKind::Char);
        assert_eq!(rf.get(Some('a')).content(), "alpha");
        assert_eq!(rf.get(Some('b')).content(), "bravo");
        // Unnamed has the most recent.
        assert_eq!(rf.get(None).content(), "bravo");
    }

    #[test]
    fn yank_named_overwrites() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "first".into(), RegisterKind::Char);
        rf.yank(Some('a'), "second".into(), RegisterKind::Line);
        assert_eq!(rf.get(Some('a')).content(), "second");
        assert_eq!(rf.get(Some('a')).kind(), RegisterKind::Line);
    }

    #[test]
    fn yank_uppercase_appends() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "hello".into(), RegisterKind::Char);
        rf.yank(Some('A'), " world".into(), RegisterKind::Char);
        assert_eq!(rf.get(Some('a')).content(), "hello world");
        assert_eq!(rf.get(Some('a')).kind(), RegisterKind::Char);
        // Unnamed gets the full appended content.
        assert_eq!(rf.get(None).content(), "hello world");
    }

    #[test]
    fn yank_uppercase_appends_linewise() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "line one\n".into(), RegisterKind::Line);
        rf.yank(Some('A'), "line two\n".into(), RegisterKind::Line);
        assert_eq!(rf.get(Some('a')).content(), "line one\nline two\n");
        assert_eq!(rf.get(Some('a')).kind(), RegisterKind::Line);
    }

    #[test]
    fn yank_uppercase_to_empty_register() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('A'), "first".into(), RegisterKind::Char);
        assert_eq!(rf.get(Some('a')).content(), "first");
    }

    #[test]
    fn get_uppercase_reads_lowercase() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('z'), "data".into(), RegisterKind::Char);
        assert_eq!(rf.get(Some('Z')).content(), "data");
    }

    #[test]
    fn get_unknown_falls_back_to_unnamed() {
        let mut rf = RegisterFile::new();
        rf.yank(None, "unnamed".into(), RegisterKind::Char);
        assert_eq!(rf.get(Some('!')).content(), "unnamed");
    }

    #[test]
    fn yank_unknown_falls_back_to_unnamed() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('!'), "fallback".into(), RegisterKind::Char);
        assert_eq!(rf.get(None).content(), "fallback");
    }

    #[test]
    fn all_26_named_registers() {
        let mut rf = RegisterFile::new();
        for (i, ch) in ('a'..='z').enumerate() {
            rf.yank(Some(ch), format!("reg_{i}"), RegisterKind::Char);
        }
        for (i, ch) in ('a'..='z').enumerate() {
            assert_eq!(rf.get(Some(ch)).content(), format!("reg_{i}"));
        }
    }

    #[test]
    fn unnamed_yank_does_not_affect_named() {
        let mut rf = RegisterFile::new();
        rf.yank(Some('a'), "named".into(), RegisterKind::Char);
        rf.yank(None, "unnamed".into(), RegisterKind::Line);
        // Named register unchanged.
        assert_eq!(rf.get(Some('a')).content(), "named");
        // Unnamed updated.
        assert_eq!(rf.get(None).content(), "unnamed");
    }
}
