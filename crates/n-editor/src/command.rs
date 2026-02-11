//! Command-line mode — the `:` prompt at the bottom of the screen.
//!
//! When the user presses `:` in normal mode, the editor enters command mode.
//! A command line appears at the bottom of the screen where the user types a
//! command. Enter executes it, Escape cancels.
//!
//! # Supported commands
//!
//! | Command     | Action                                         |
//! |-------------|------------------------------------------------|
//! | `:w`        | Save to current file path                      |
//! | `:w <path>` | Save to a specific path (save-as)              |
//! | `:q`        | Quit (fails if buffer is modified)              |
//! | `:q!`       | Force quit (discard changes)                    |
//! | `:wq`       | Save and quit                                  |
//! | `:x`        | Save (only if modified) and quit                |
//!
//! # Architecture
//!
//! The command line is a simple string buffer with cursor position. Commands
//! are parsed into a [`Command`] enum after the user presses Enter. The
//! editor then executes the command and handles the result.

use std::path::PathBuf;

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

    // Split into command and arguments.
    let (cmd, arg) = trimmed
        .find(char::is_whitespace)
        .map_or((trimmed, ""), |pos| {
            (&trimmed[..pos], trimmed[pos..].trim_start())
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

    // -- Command parsing ----------------------------------------------------

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
}
