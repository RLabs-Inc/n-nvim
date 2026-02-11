//! Command-line mode — the `:` prompt at the bottom of the screen.
//!
//! When the user presses `:` in normal mode, the editor enters command mode.
//! A command line appears at the bottom of the screen where the user types a
//! command. Enter executes it, Escape cancels.
//!
//! # Supported commands
//!
//! | Command                    | Action                                  |
//! |----------------------------|-----------------------------------------|
//! | `:w`                       | Save to current file path               |
//! | `:w <path>`                | Save to a specific path (save-as)       |
//! | `:q`                       | Quit (fails if buffer is modified)       |
//! | `:q!`                      | Force quit (discard changes)             |
//! | `:wq`                      | Save and quit                           |
//! | `:x`                       | Save (only if modified) and quit         |
//! | `:s/pat/rep/flags`         | Substitute on current line              |
//! | `:%s/pat/rep/flags`        | Substitute on all lines                 |
//! | `:N,Ms/pat/rep/flags`      | Substitute on line range                |
//! | `:'<,'>s/pat/rep/flags`    | Substitute on visual selection          |
//! | `:s`                       | Repeat last substitution                |
//!
//! # Substitution flags
//!
//! | Flag | Effect                               |
//! |------|--------------------------------------|
//! | `g`  | Replace all matches per line          |
//! | `i`  | Case-insensitive matching             |
//! | `n`  | Count matches only (don't replace)    |
//!
//! # Architecture
//!
//! The command line is a simple string buffer with cursor position. Commands
//! are parsed into a [`Command`] enum after the user presses Enter. The
//! editor then executes the command and handles the result.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Command-line range
// ---------------------------------------------------------------------------

/// An address range prefix for commands like `:s`.
///
/// Vim commands can be prefixed with a range specifying which lines to
/// operate on. This enum represents the parsed range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdRange {
    /// No range given — default depends on the command (usually current line).
    CurrentLine,

    /// `%` — the entire file.
    All,

    /// `N,M` — explicit line numbers (0-indexed internally; Vim's 1-indexed
    /// input is converted during parsing).
    Lines(usize, usize),

    /// `'<,'>` — the last visual selection.
    Visual,
}

// ---------------------------------------------------------------------------
// Substitution flags
// ---------------------------------------------------------------------------

/// Flags for the `:s` substitution command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubFlags {
    /// `g` — replace all matches on each line (not just the first).
    pub global: bool,

    /// `i` — case-insensitive matching.
    pub case_insensitive: bool,

    /// `n` — count matches only, don't actually replace.
    pub count_only: bool,
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// A parsed command-line command.
///
/// Produced by [`CommandLine::parse`] after the user presses Enter. The editor
/// matches on this enum to execute the appropriate action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `:w` — save to the current path.
    Write,

    /// `:w <path>` — save to a specific path.
    WriteAs(PathBuf),

    /// `:q` — quit (refuses if buffer is modified).
    Quit,

    /// `:q!` — force quit (discards unsaved changes).
    ForceQuit,

    /// `:wq` — save and quit.
    WriteQuit,

    /// `:x` — save if modified, then quit.
    ExitSave,

    /// `:[range]s/pattern/replacement/[flags]` — substitute.
    Substitute {
        range: CmdRange,
        pattern: String,
        replacement: String,
        flags: SubFlags,
    },

    /// `:s` (no arguments) — repeat the last substitution.
    SubRepeat {
        range: CmdRange,
    },

    /// Unknown command — contains the full input for error reporting.
    Unknown(String),
}

/// The result of executing a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// Command succeeded. Optional message for the status line.
    Ok(Option<String>),

    /// Command failed. Error message for the status line.
    Err(String),

    /// Editor should quit.
    Quit,
}

// ---------------------------------------------------------------------------
// CommandLine
// ---------------------------------------------------------------------------

/// The command-line input buffer.
///
/// Tracks the text the user is typing and the cursor position within it.
/// The leading `:` is not stored — it's rendered by the view layer.
#[derive(Debug, Clone)]
pub struct CommandLine {
    /// The command text (without the leading `:`).
    input: String,

    /// Cursor position within `input` (char offset, 0-indexed).
    cursor: usize,
}

impl CommandLine {
    /// Create an empty command line.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
        }
    }

    /// The current input text (without the leading `:`).
    #[inline]
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The cursor position within the input (char offset).
    #[inline]
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, ch: char) {
        let byte_idx = self.char_to_byte(self.cursor);
        self.input.insert(byte_idx, ch);
        self.cursor += 1;
    }

    /// Delete the character before the cursor (backspace).
    /// Returns `true` if a character was deleted.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        let byte_idx = self.char_to_byte(self.cursor);
        self.input.remove(byte_idx);
        true
    }

    /// Delete the character at the cursor (delete key).
    /// Returns `true` if a character was deleted.
    pub fn delete(&mut self) -> bool {
        let len = self.input.chars().count();
        if self.cursor >= len {
            return false;
        }
        let byte_idx = self.char_to_byte(self.cursor);
        self.input.remove(byte_idx);
        true
    }

    /// Move the cursor one position to the left.
    pub const fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one position to the right.
    pub fn move_right(&mut self) {
        let len = self.input.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    /// Move the cursor to the beginning.
    pub const fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end.
    pub fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    /// Clear the input and reset the cursor.
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// True if the input is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// Parse the current input into a [`Command`].
    #[must_use]
    pub fn parse(&self) -> Command {
        parse_command(&self.input)
    }

    /// Convert a char offset to a byte offset in `self.input`.
    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(byte_idx, _)| byte_idx)
    }
}

impl Default for CommandLine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a command string (without the leading `:`) into a [`Command`].
fn parse_command(input: &str) -> Command {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Command::Unknown(String::new());
    }

    // Try to parse a range prefix, then the command.
    let (range, rest) = parse_range(trimmed);

    // If there's a range and the next char is `s`, it's a substitution.
    if let Some(after_s) = rest.strip_prefix('s') {
        return parse_substitute(range, after_s);
    }

    // A range with no command following it is invalid.
    if !matches!(range, CmdRange::CurrentLine) && rest.is_empty() {
        return Command::Unknown(trimmed.to_string());
    }

    // Fall through to standard command parsing (ranges ignored for non-range
    // commands like :w, :q, etc.).
    let cmd_str = if rest.is_empty() { trimmed } else { rest };

    // Split into command and arguments.
    let (cmd, arg) = cmd_str
        .find(char::is_whitespace)
        .map_or((cmd_str, ""), |pos| {
            (&cmd_str[..pos], cmd_str[pos..].trim_start())
        });

    match cmd {
        "w" => {
            if arg.is_empty() {
                Command::Write
            } else {
                Command::WriteAs(PathBuf::from(arg))
            }
        }
        "q" => Command::Quit,
        "q!" => Command::ForceQuit,
        "wq" => Command::WriteQuit,
        "x" => Command::ExitSave,
        _ => Command::Unknown(trimmed.to_string()),
    }
}

/// Parse a range prefix from the start of a command string.
///
/// Returns `(range, rest)` where `rest` is the command string after the range.
/// If no range is found, returns `(CmdRange::CurrentLine, input)`.
fn parse_range(input: &str) -> (CmdRange, &str) {
    let bytes = input.as_bytes();

    if bytes.is_empty() {
        return (CmdRange::CurrentLine, input);
    }

    // `%` — entire file.
    if bytes[0] == b'%' {
        return (CmdRange::All, &input[1..]);
    }

    // `'<,'>` — visual selection.
    if let Some(rest) = input.strip_prefix("'<,'>") {
        return (CmdRange::Visual, rest);
    }

    // `N,M` — line range (1-indexed input → 0-indexed internal).
    if bytes[0].is_ascii_digit() {
        if let Some((start, rest_after_start)) = parse_line_number(input) {
            if let Some(after_comma) = rest_after_start.strip_prefix(',') {
                if let Some((end, rest_after_end)) = parse_line_number(after_comma) {
                    // Convert from 1-indexed to 0-indexed.
                    let start_0 = start.saturating_sub(1);
                    let end_0 = end.saturating_sub(1);
                    return (CmdRange::Lines(start_0, end_0), rest_after_end);
                }
            }
        }
    }

    (CmdRange::CurrentLine, input)
}

/// Parse a decimal number from the start of `input`.
///
/// Returns `(number, rest)` or `None` if the input doesn't start with a digit.
fn parse_line_number(input: &str) -> Option<(usize, &str)> {
    let end = input
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    let num: usize = input[..end].parse().ok()?;
    Some((num, &input[end..]))
}

/// Parse the body of a `:s` command (everything after the `s`).
///
/// Handles `s/pattern/replacement/flags` with arbitrary delimiters and
/// escaped delimiters (e.g., `s#foo#bar#g` or `s/foo\/bar/baz/`).
///
/// If the body is empty, returns `SubRepeat` (repeat last substitution).
fn parse_substitute(range: CmdRange, body: &str) -> Command {
    // `:s` with no body = repeat last substitution.
    if body.is_empty() {
        return Command::SubRepeat { range };
    }

    // The first character is the delimiter.
    let mut chars = body.chars();
    let delim = chars.next().unwrap();

    // Collect the rest after the delimiter.
    let after_delim = &body[delim.len_utf8()..];

    // Parse pattern.
    let Some((pattern, rest)) = split_at_unescaped(after_delim, delim) else {
        // No closing delimiter — pattern-only, empty replacement, no flags.
        return Command::Substitute {
            range,
            pattern: unescape_delim(after_delim, delim),
            replacement: String::new(),
            flags: SubFlags::default(),
        };
    };

    // Parse replacement.
    let Some((replacement, rest)) = split_at_unescaped(rest, delim) else {
        // No trailing delimiter — rest is the replacement, no flags.
        return Command::Substitute {
            range,
            pattern: unescape_delim(pattern, delim),
            replacement: unescape_delim(rest, delim),
            flags: SubFlags::default(),
        };
    };

    // Parse flags from the remainder.
    let flags = parse_sub_flags(rest);

    Command::Substitute {
        range,
        pattern: unescape_delim(pattern, delim),
        replacement: unescape_delim(replacement, delim),
        flags,
    }
}

/// Split a string at the first unescaped occurrence of `delim`.
///
/// `\<delim>` is treated as an escaped delimiter and not a split point.
/// Returns `Some((before, after))` or `None` if no unescaped delimiter found.
fn split_at_unescaped(s: &str, delim: char) -> Option<(&str, &str)> {
    let mut escaped = false;
    for (byte_idx, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == delim {
            return Some((&s[..byte_idx], &s[byte_idx + ch.len_utf8()..]));
        }
    }
    None
}

/// Remove escape sequences for the delimiter character.
///
/// `\<delim>` → `<delim>`, all other `\X` sequences pass through unchanged
/// (they'll be interpreted by the regex engine or replacement handler).
fn unescape_delim(s: &str, delim: char) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if next == delim {
                    result.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        result.push(ch);
    }
    result
}

/// Parse substitution flags from a string (e.g., `"gi"` → global + case-insensitive).
fn parse_sub_flags(s: &str) -> SubFlags {
    let mut flags = SubFlags::default();
    for ch in s.chars() {
        match ch {
            'g' => flags.global = true,
            'i' => flags.case_insensitive = true,
            'n' => flags.count_only = true,
            _ => {} // Unknown flags silently ignored (like Vim).
        }
    }
    flags
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- CommandLine basic operations ---------------------------------------

    #[test]
    fn new_is_empty() {
        let cl = CommandLine::new();
        assert!(cl.is_empty());
        assert_eq!(cl.input(), "");
        assert_eq!(cl.cursor(), 0);
    }

    #[test]
    fn default_is_new() {
        let cl = CommandLine::default();
        assert!(cl.is_empty());
    }

    #[test]
    fn insert_chars() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        assert_eq!(cl.input(), "wq");
        assert_eq!(cl.cursor(), 2);
    }

    #[test]
    fn insert_in_middle() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        cl.move_left();
        cl.insert_char('!');
        assert_eq!(cl.input(), "w!q");
        assert_eq!(cl.cursor(), 2);
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        assert!(cl.backspace());
        assert_eq!(cl.input(), "w");
        assert_eq!(cl.cursor(), 1);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut cl = CommandLine::new();
        assert!(!cl.backspace());
        assert_eq!(cl.cursor(), 0);
    }

    #[test]
    fn delete_at_cursor() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        cl.move_home();
        assert!(cl.delete());
        assert_eq!(cl.input(), "q");
        assert_eq!(cl.cursor(), 0);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        assert!(!cl.delete());
        assert_eq!(cl.input(), "w");
    }

    // -- Cursor movement ----------------------------------------------------

    #[test]
    fn move_left_and_right() {
        let mut cl = CommandLine::new();
        cl.insert_char('a');
        cl.insert_char('b');
        cl.insert_char('c');
        assert_eq!(cl.cursor(), 3);

        cl.move_left();
        assert_eq!(cl.cursor(), 2);
        cl.move_left();
        assert_eq!(cl.cursor(), 1);
        cl.move_right();
        assert_eq!(cl.cursor(), 2);
    }

    #[test]
    fn move_left_stops_at_zero() {
        let mut cl = CommandLine::new();
        cl.insert_char('a');
        cl.move_left();
        cl.move_left();
        cl.move_left();
        assert_eq!(cl.cursor(), 0);
    }

    #[test]
    fn move_right_stops_at_end() {
        let mut cl = CommandLine::new();
        cl.insert_char('a');
        cl.move_right();
        cl.move_right();
        assert_eq!(cl.cursor(), 1);
    }

    #[test]
    fn move_home_and_end() {
        let mut cl = CommandLine::new();
        cl.insert_char('a');
        cl.insert_char('b');
        cl.insert_char('c');
        cl.move_home();
        assert_eq!(cl.cursor(), 0);
        cl.move_end();
        assert_eq!(cl.cursor(), 3);
    }

    // -- Clear --------------------------------------------------------------

    #[test]
    fn clear_resets_all() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        cl.clear();
        assert!(cl.is_empty());
        assert_eq!(cl.cursor(), 0);
    }

    // -- Unicode handling ---------------------------------------------------

    #[test]
    fn unicode_insert_and_backspace() {
        let mut cl = CommandLine::new();
        cl.insert_char('é');
        cl.insert_char('à');
        assert_eq!(cl.input(), "éà");
        assert_eq!(cl.cursor(), 2);

        cl.backspace();
        assert_eq!(cl.input(), "é");
        assert_eq!(cl.cursor(), 1);
    }

    #[test]
    fn unicode_cursor_movement() {
        let mut cl = CommandLine::new();
        cl.insert_char('日');
        cl.insert_char('本');
        assert_eq!(cl.cursor(), 2);

        cl.move_left();
        assert_eq!(cl.cursor(), 1);
        cl.insert_char('中');
        assert_eq!(cl.input(), "日中本");
        assert_eq!(cl.cursor(), 2);
    }

    // -- Command parsing (standard) ----------------------------------------

    #[test]
    fn parse_write() {
        assert_eq!(parse_command("w"), Command::Write);
    }

    #[test]
    fn parse_write_with_path() {
        assert_eq!(
            parse_command("w /tmp/test.txt"),
            Command::WriteAs(PathBuf::from("/tmp/test.txt"))
        );
    }

    #[test]
    fn parse_write_with_spaces_in_path() {
        assert_eq!(
            parse_command("w /tmp/my file.txt"),
            Command::WriteAs(PathBuf::from("/tmp/my file.txt"))
        );
    }

    #[test]
    fn parse_quit() {
        assert_eq!(parse_command("q"), Command::Quit);
    }

    #[test]
    fn parse_force_quit() {
        assert_eq!(parse_command("q!"), Command::ForceQuit);
    }

    #[test]
    fn parse_write_quit() {
        assert_eq!(parse_command("wq"), Command::WriteQuit);
    }

    #[test]
    fn parse_exit_save() {
        assert_eq!(parse_command("x"), Command::ExitSave);
    }

    #[test]
    fn parse_unknown() {
        assert_eq!(
            parse_command("foobar"),
            Command::Unknown("foobar".to_string())
        );
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse_command(""), Command::Unknown(String::new()));
    }

    #[test]
    fn parse_whitespace_only() {
        assert_eq!(parse_command("   "), Command::Unknown(String::new()));
    }

    #[test]
    fn parse_leading_trailing_whitespace() {
        assert_eq!(parse_command("  w  "), Command::Write);
        assert_eq!(parse_command("  q!  "), Command::ForceQuit);
    }

    #[test]
    fn parse_via_command_line() {
        let mut cl = CommandLine::new();
        cl.insert_char('w');
        cl.insert_char('q');
        assert_eq!(cl.parse(), Command::WriteQuit);
    }

    // -- CommandResult variants exist ---------------------------------------

    #[test]
    fn command_result_ok_with_message() {
        let r = CommandResult::Ok(Some("Written 42 bytes".to_string()));
        assert!(matches!(r, CommandResult::Ok(Some(_))));
    }

    #[test]
    fn command_result_ok_no_message() {
        let r = CommandResult::Ok(None);
        assert!(matches!(r, CommandResult::Ok(None)));
    }

    #[test]
    fn command_result_err() {
        let r = CommandResult::Err("No file name".to_string());
        assert!(matches!(r, CommandResult::Err(_)));
    }

    #[test]
    fn command_result_quit() {
        let r = CommandResult::Quit;
        assert!(matches!(r, CommandResult::Quit));
    }

    // -- Range parsing ------------------------------------------------------

    #[test]
    fn range_none() {
        let (range, rest) = parse_range("s/foo/bar/");
        assert_eq!(range, CmdRange::CurrentLine);
        assert_eq!(rest, "s/foo/bar/");
    }

    #[test]
    fn range_percent() {
        let (range, rest) = parse_range("%s/foo/bar/");
        assert_eq!(range, CmdRange::All);
        assert_eq!(rest, "s/foo/bar/");
    }

    #[test]
    fn range_visual() {
        let (range, rest) = parse_range("'<,'>s/foo/bar/");
        assert_eq!(range, CmdRange::Visual);
        assert_eq!(rest, "s/foo/bar/");
    }

    #[test]
    fn range_line_numbers() {
        let (range, rest) = parse_range("5,10s/foo/bar/");
        assert_eq!(range, CmdRange::Lines(4, 9)); // 0-indexed
        assert_eq!(rest, "s/foo/bar/");
    }

    #[test]
    fn range_line_numbers_single_digit() {
        let (range, rest) = parse_range("1,1s/a/b/");
        assert_eq!(range, CmdRange::Lines(0, 0));
        assert_eq!(rest, "s/a/b/");
    }

    #[test]
    fn range_line_number_large() {
        let (range, rest) = parse_range("100,200s/x/y/");
        assert_eq!(range, CmdRange::Lines(99, 199));
        assert_eq!(rest, "s/x/y/");
    }

    // -- Substitution parsing -----------------------------------------------

    #[test]
    fn sub_basic() {
        assert_eq!(
            parse_command("s/foo/bar/"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_no_trailing_delimiter() {
        // Vim allows omitting the trailing delimiter.
        assert_eq!(
            parse_command("s/foo/bar"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_global_flag() {
        assert_eq!(
            parse_command("s/foo/bar/g"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_case_insensitive() {
        assert_eq!(
            parse_command("s/foo/bar/i"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { case_insensitive: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_count_only() {
        assert_eq!(
            parse_command("s/foo/bar/n"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { count_only: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_multiple_flags() {
        assert_eq!(
            parse_command("s/foo/bar/gi"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { global: true, case_insensitive: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_percent_range() {
        assert_eq!(
            parse_command("%s/foo/bar/g"),
            Command::Substitute {
                range: CmdRange::All,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_line_range() {
        assert_eq!(
            parse_command("1,5s/foo/bar/"),
            Command::Substitute {
                range: CmdRange::Lines(0, 4),
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_visual_range() {
        assert_eq!(
            parse_command("'<,'>s/foo/bar/g"),
            Command::Substitute {
                range: CmdRange::Visual,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_alternate_delimiter() {
        assert_eq!(
            parse_command("s#foo#bar#g"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "bar".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_escaped_delimiter() {
        // s/foo\/bar/baz/ — pattern is "foo/bar", replacement is "baz".
        assert_eq!(
            parse_command(r"s/foo\/bar/baz/"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo/bar".to_string(),
                replacement: "baz".to_string(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_empty_replacement() {
        // `:s/foo//` — delete all occurrences of "foo".
        assert_eq!(
            parse_command("s/foo//"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: String::new(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_empty_replacement_with_flags() {
        assert_eq!(
            parse_command("s/foo//g"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: String::new(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_pattern_only() {
        // s/foo — pattern only, empty replacement, no flags.
        assert_eq!(
            parse_command("s/foo"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: String::new(),
                flags: SubFlags::default(),
            }
        );
    }

    #[test]
    fn sub_repeat_no_args() {
        assert_eq!(
            parse_command("s"),
            Command::SubRepeat { range: CmdRange::CurrentLine }
        );
    }

    #[test]
    fn sub_repeat_with_range() {
        assert_eq!(
            parse_command("%s"),
            Command::SubRepeat { range: CmdRange::All }
        );
    }

    #[test]
    fn sub_backslash_in_replacement() {
        // Backslashes that aren't escaping the delimiter pass through.
        assert_eq!(
            parse_command(r"s/(\w+)/\1/g"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: r"(\w+)".to_string(),
                replacement: r"\1".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    #[test]
    fn sub_ampersand_in_replacement() {
        // `&` in the replacement passes through (handled at execution time).
        assert_eq!(
            parse_command("s/foo/[&]/g"),
            Command::Substitute {
                range: CmdRange::CurrentLine,
                pattern: "foo".to_string(),
                replacement: "[&]".to_string(),
                flags: SubFlags { global: true, ..SubFlags::default() },
            }
        );
    }

    // -- split_at_unescaped / unescape_delim helpers ------------------------

    #[test]
    fn split_basic() {
        assert_eq!(split_at_unescaped("foo/bar", '/'), Some(("foo", "bar")));
    }

    #[test]
    fn split_escaped() {
        assert_eq!(split_at_unescaped(r"foo\/bar/baz", '/'), Some((r"foo\/bar", "baz")));
    }

    #[test]
    fn split_no_delimiter() {
        assert_eq!(split_at_unescaped("foobar", '/'), None);
    }

    #[test]
    fn split_empty() {
        assert_eq!(split_at_unescaped("/rest", '/'), Some(("", "rest")));
    }

    #[test]
    fn unescape_basic() {
        assert_eq!(unescape_delim(r"foo\/bar", '/'), "foo/bar");
    }

    #[test]
    fn unescape_no_escapes() {
        assert_eq!(unescape_delim("foobar", '/'), "foobar");
    }

    #[test]
    fn unescape_preserves_other_backslashes() {
        assert_eq!(unescape_delim(r"\1\n\\", '/'), r"\1\n\\");
    }

    // -- SubFlags parsing ---------------------------------------------------

    #[test]
    fn flags_empty() {
        assert_eq!(parse_sub_flags(""), SubFlags::default());
    }

    #[test]
    fn flags_all() {
        assert_eq!(
            parse_sub_flags("gin"),
            SubFlags { global: true, case_insensitive: true, count_only: true }
        );
    }

    #[test]
    fn flags_unknown_ignored() {
        assert_eq!(
            parse_sub_flags("gxz"),
            SubFlags { global: true, ..SubFlags::default() }
        );
    }
}
