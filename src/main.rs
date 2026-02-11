// SPDX-License-Identifier: MIT
//
// n-nvim — a terminal text editor that reimagines Neovim.
//
// This is the main binary that wires together all the crates:
//
//   n-term   → terminal control, rendering, input parsing, event loop
//   n-editor → text buffer, cursor, modes, view layer
//
// The Editor struct implements n-term's App trait, connecting the event
// loop to the editor's state. Each keypress flows through:
//
//   stdin → parser → on_event → mode dispatch → buffer/cursor mutation
//   paint → view.render → framebuffer → diff renderer → terminal
//
// Layout:
//
//   ┌──────────────────────────────┐
//   │ text area + gutter           │  ← h - 2 rows (managed by View)
//   ├──────────────────────────────┤
//   │ status line (INVERSE)        │  ← 1 row (managed by View)
//   ├──────────────────────────────┤
//   │ command / message line       │  ← 1 row (managed by Editor)
//   └──────────────────────────────┘

use std::env;
use std::path::{Path, PathBuf};
use std::process;

use n_editor::buffer::Buffer;
use n_editor::command::{Command, CommandLine, CommandResult};
use n_editor::cursor::Cursor;
use n_editor::history::History;
use n_editor::mode::Mode;
use n_editor::position::{Position, Range};
use n_editor::view::{self, View};

use n_term::ansi::CursorShape;
use n_term::buffer::FrameBuffer;
use n_term::event_loop::{Action, App, EventLoop};
use n_term::input::{Event, KeyCode, KeyEvent, Modifiers};
use n_term::terminal::Size;

// ─── Editor ─────────────────────────────────────────────────────────────────

/// The editor application state.
///
/// Holds everything needed to edit a file: the text buffer, cursor position,
/// current mode, undo history, view configuration, command line state, and
/// the screen position of the cursor computed during the last paint.
struct Editor {
    buffer: Buffer,
    cursor: Cursor,
    view: View,
    mode: Mode,
    history: History,

    /// Pending operator key for multi-key commands like `dd`. When `d` is
    /// pressed, this is set to `Some('d')`. The next keypress completes or
    /// cancels the operator.
    pending_op: Option<char>,

    /// Screen position of the cursor from the last paint, used by the
    /// event loop to position the hardware terminal cursor.
    cursor_screen: Option<(u16, u16)>,

    /// The command-line input buffer (active when mode == Command).
    cmdline: CommandLine,

    /// A message to display on the bottom line. Cleared on the next keypress.
    message: Option<String>,

    /// Whether the current message is an error (renders in red).
    message_is_error: bool,
}

impl Editor {
    /// Create an editor with an empty buffer.
    fn new() -> Self {
        Self {
            buffer: Buffer::new(),
            cursor: Cursor::new(),
            view: View::new(),
            mode: Mode::Normal,
            history: History::new(),
            pending_op: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            message: None,
            message_is_error: false,
        }
    }

    /// Create an editor with a file loaded from disk.
    fn from_file(path: &str) -> Self {
        let path_buf = PathBuf::from(path);
        let buffer = Buffer::from_file(&path_buf).unwrap_or_else(|e| {
            eprintln!("n-nvim: {path}: {e}");
            process::exit(1);
        });
        Self {
            buffer,
            cursor: Cursor::new(),
            view: View::new(),
            mode: Mode::Normal,
            history: History::new(),
            pending_op: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            message: None,
            message_is_error: false,
        }
    }

    /// Set a success message on the bottom line.
    fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_is_error = false;
    }

    /// Set an error message on the bottom line.
    fn set_error(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_is_error = true;
    }

    /// Clear any displayed message.
    fn clear_message(&mut self) {
        self.message = None;
        self.message_is_error = false;
    }

    // ── Normal mode ──────────────────────────────────────────────────────

    fn handle_normal(&mut self, key: &KeyEvent) -> Action {
        // Any keypress in normal mode clears the message line.
        self.clear_message();

        let pe = self.mode.cursor_past_end();

        // Ctrl combinations handled first.
        if key.modifiers.contains(Modifiers::CTRL) {
            match key.code {
                KeyCode::Char('c') => return Action::Quit,
                KeyCode::Char('r') => {
                    self.pending_op = None;
                    if let Some(pos) = self.history.redo(&mut self.buffer) {
                        self.cursor.set_position(pos, &self.buffer, pe);
                    }
                    return Action::Continue;
                }
                _ => {}
            }
        }

        // Handle pending operator (e.g. the second key in `dd`).
        if let Some(op) = self.pending_op.take() {
            if op == 'd' && key.code == KeyCode::Char('d') {
                self.delete_current_line();
                return Action::Continue;
            }
            // Unknown operator sequence — discard and fall through
            // so the key is processed normally.
        }

        self.handle_normal_key(key, pe)
    }

    /// Process a single normal-mode key (after Ctrl and pending-op handling).
    fn handle_normal_key(&mut self, key: &KeyEvent, pe: bool) -> Action {
        match key.code {
            // -- Enter command mode --
            KeyCode::Char(':') => {
                self.cmdline.clear();
                self.mode = Mode::Command;
            }

            // -- Mode transitions (all begin a history transaction) --
            KeyCode::Char('i') => {
                self.history.begin(self.cursor.position());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('a') => {
                self.history.begin(self.cursor.position());
                self.cursor.move_right(1, &self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                self.history.begin(self.cursor.position());
                self.cursor.move_to_line_end(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                self.history.begin(self.cursor.position());
                self.cursor.move_to_first_non_blank(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => self.open_line_below(),
            KeyCode::Char('O') => self.open_line_above(),

            // -- Delete operations --
            KeyCode::Char('x') => {
                self.delete_char_at_cursor();
            }
            KeyCode::Char('d') => {
                self.pending_op = Some('d');
            }

            // -- Undo/redo --
            KeyCode::Char('u') => {
                if let Some(pos) = self.history.undo(&mut self.buffer) {
                    self.cursor.set_position(pos, &self.buffer, pe);
                }
            }

            // -- Basic movement --
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor.move_left(1, &self.buffer, pe);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor.move_right(1, &self.buffer, pe);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.move_down(1, &self.buffer, pe);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.move_up(1, &self.buffer, pe);
            }

            // -- Line motions --
            KeyCode::Char('0') | KeyCode::Home => {
                self.cursor.move_to_line_start();
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.cursor.move_to_line_end(&self.buffer, pe);
            }
            KeyCode::Char('^') => {
                self.cursor.move_to_first_non_blank(&self.buffer, pe);
            }

            // -- Word motions --
            KeyCode::Char('w') => {
                self.cursor.word_forward(1, &self.buffer, pe);
            }
            KeyCode::Char('b') => {
                self.cursor.word_backward(1, &self.buffer, pe);
            }
            KeyCode::Char('e') => {
                self.cursor.word_end_forward(1, &self.buffer, pe);
            }
            KeyCode::Char('W') => {
                self.cursor.big_word_forward(1, &self.buffer, pe);
            }
            KeyCode::Char('B') => {
                self.cursor.big_word_backward(1, &self.buffer, pe);
            }
            KeyCode::Char('E') => {
                self.cursor.big_word_end_forward(1, &self.buffer, pe);
            }

            // -- File motions --
            KeyCode::Char('g') => {
                // gg = go to first line. We consume 'g' here; a full
                // implementation would use a pending-key state machine.
                // For now, single 'g' acts as 'gg'.
                self.cursor.move_to_first_line(&self.buffer, pe);
            }
            KeyCode::Char('G') => {
                self.cursor.move_to_last_line(&self.buffer, pe);
            }

            _ => {}
        }

        Action::Continue
    }

    // ── Insert mode ─────────────────────────────────────────────────────

    fn handle_insert(&mut self, key: &KeyEvent) -> Action {
        // Clear message on first keypress in insert mode.
        self.clear_message();

        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        match key.code {
            KeyCode::Escape => {
                // Commit the insert-mode transaction and return to normal.
                self.history.commit(self.cursor.position());
                self.mode = Mode::Normal;
                self.cursor.move_left(1, &self.buffer, false);
            }

            KeyCode::Char(ch) => {
                let pos = self.cursor.position();
                self.buffer.insert_char(pos, ch);
                self.history.record_insert(pos, &ch.to_string());
                self.cursor.move_right(1, &self.buffer, true);
            }

            KeyCode::Enter => {
                let pos = self.cursor.position();
                self.buffer.insert_char(pos, '\n');
                self.history.record_insert(pos, "\n");
                self.cursor
                    .set_position(Position::new(pos.line + 1, 0), &self.buffer, true);
            }

            KeyCode::Backspace => {
                let pos = self.cursor.position();
                if pos.col > 0 {
                    let from = Position::new(pos.line, pos.col - 1);
                    let ch = self.buffer.char_at(from).unwrap();
                    self.history.record_delete(from, &ch.to_string());
                    self.buffer.delete(Range::new(from, pos));
                    self.cursor.set_position(from, &self.buffer, true);
                } else if pos.line > 0 {
                    // Join with previous line — delete the newline.
                    let prev_line = pos.line - 1;
                    let prev_len = self.buffer.line_content_len(prev_line).unwrap_or(0);
                    let from = Position::new(prev_line, prev_len);
                    let range = Range::new(from, pos);
                    let deleted = self
                        .buffer
                        .slice(range)
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    self.history.record_delete(from, &deleted);
                    self.buffer.delete(range);
                    self.cursor.set_position(from, &self.buffer, true);
                }
            }

            KeyCode::Delete => {
                let pos = self.cursor.position();
                let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);
                if pos.col < line_len {
                    let to = Position::new(pos.line, pos.col + 1);
                    let ch = self.buffer.char_at(pos).unwrap();
                    self.history.record_delete(pos, &ch.to_string());
                    self.buffer.delete(Range::new(pos, to));
                } else if pos.line + 1 < self.buffer.line_count() {
                    // At end of line: join with next line.
                    let to = Position::new(pos.line + 1, 0);
                    let range = Range::new(pos, to);
                    let deleted = self
                        .buffer
                        .slice(range)
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    self.history.record_delete(pos, &deleted);
                    self.buffer.delete(range);
                }
            }

            // Arrow keys work in insert mode too (no history needed).
            KeyCode::Left => self.cursor.move_left(1, &self.buffer, true),
            KeyCode::Right => self.cursor.move_right(1, &self.buffer, true),
            KeyCode::Up => self.cursor.move_up(1, &self.buffer, true),
            KeyCode::Down => self.cursor.move_down(1, &self.buffer, true),
            KeyCode::Home => self.cursor.move_to_line_start(),
            KeyCode::End => self.cursor.move_to_line_end(&self.buffer, true),

            _ => {}
        }

        Action::Continue
    }

    // ── Command mode ────────────────────────────────────────────────────

    fn handle_command(&mut self, key: &KeyEvent) -> Action {
        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            // Ctrl-C cancels command mode (same as Escape).
            self.mode = Mode::Normal;
            self.cmdline.clear();
            return Action::Continue;
        }

        match key.code {
            KeyCode::Escape => {
                // Cancel command mode.
                self.mode = Mode::Normal;
                self.cmdline.clear();
            }

            KeyCode::Enter => {
                // Parse and execute the command.
                let cmd = self.cmdline.parse();
                self.mode = Mode::Normal;
                self.cmdline.clear();
                return self.execute_command(cmd);
            }

            KeyCode::Char(ch) => {
                self.cmdline.insert_char(ch);
            }

            KeyCode::Backspace => {
                if !self.cmdline.backspace() {
                    // Backspace on empty command line cancels (like Vim).
                    self.mode = Mode::Normal;
                }
            }

            KeyCode::Delete => {
                self.cmdline.delete();
            }

            KeyCode::Left => self.cmdline.move_left(),
            KeyCode::Right => self.cmdline.move_right(),
            KeyCode::Home => self.cmdline.move_home(),
            KeyCode::End => self.cmdline.move_end(),

            _ => {}
        }

        Action::Continue
    }

    /// Execute a parsed command and return the appropriate action.
    fn execute_command(&mut self, cmd: Command) -> Action {
        match self.run_command(cmd) {
            CommandResult::Ok(Some(msg)) => {
                self.set_message(msg);
                Action::Continue
            }
            CommandResult::Ok(None) => Action::Continue,
            CommandResult::Err(msg) => {
                self.set_error(msg);
                Action::Continue
            }
            CommandResult::Quit => Action::Quit,
        }
    }

    /// Run a command and produce a result.
    fn run_command(&mut self, cmd: Command) -> CommandResult {
        match cmd {
            Command::Write => self.cmd_write(),
            Command::WriteAs(path) => self.cmd_write_as(&path),
            Command::Quit => self.cmd_quit(),
            Command::ForceQuit => CommandResult::Quit,
            Command::WriteQuit => self.cmd_write_quit(),
            Command::ExitSave => self.cmd_exit_save(),
            Command::Unknown(input) => {
                if input.is_empty() {
                    CommandResult::Ok(None)
                } else {
                    CommandResult::Err(format!("E492: Not an editor command: {input}"))
                }
            }
        }
    }

    /// `:w` — save the buffer.
    fn cmd_write(&mut self) -> CommandResult {
        if self.buffer.path().is_none() {
            return CommandResult::Err("E32: No file name".to_string());
        }
        match self.buffer.save() {
            Ok(()) => {
                let path = self
                    .buffer
                    .path()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("???");
                let bytes = self.buffer.len_bytes();
                CommandResult::Ok(Some(format!("\"{path}\" written, {bytes}B")))
            }
            Err(e) => CommandResult::Err(format!("E212: Can't save file: {e}")),
        }
    }

    /// `:w <path>` — save the buffer to a specific path.
    fn cmd_write_as(&mut self, path: &Path) -> CommandResult {
        match self.buffer.save_as(path) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("???");
                let bytes = self.buffer.len_bytes();
                CommandResult::Ok(Some(format!("\"{name}\" written, {bytes}B")))
            }
            Err(e) => CommandResult::Err(format!("E212: Can't save file: {e}")),
        }
    }

    /// `:q` — quit if no unsaved changes.
    fn cmd_quit(&self) -> CommandResult {
        if self.buffer.is_modified() {
            CommandResult::Err(
                "E37: No write since last change (add ! to override)".to_string(),
            )
        } else {
            CommandResult::Quit
        }
    }

    /// `:wq` — save and quit.
    fn cmd_write_quit(&mut self) -> CommandResult {
        match self.cmd_write() {
            CommandResult::Ok(_) => CommandResult::Quit,
            err => err,
        }
    }

    /// `:x` — save if modified, then quit.
    fn cmd_exit_save(&mut self) -> CommandResult {
        if self.buffer.is_modified() {
            self.cmd_write_quit()
        } else {
            CommandResult::Quit
        }
    }

    // ── Line-opening commands ─────────────────────────────────────────────

    /// Open a new line below the current one (`o` in Vim).
    fn open_line_below(&mut self) {
        self.history.begin(self.cursor.position());
        let line = self.cursor.line();
        let line_len = self.buffer.line_content_len(line).unwrap_or(0);
        let eol = Position::new(line, line_len);
        self.buffer.insert(eol, "\n");
        self.history.record_insert(eol, "\n");
        self.cursor
            .set_position(Position::new(line + 1, 0), &self.buffer, true);
        self.mode = Mode::Insert;
    }

    /// Open a new line above the current one (`O` in Vim).
    fn open_line_above(&mut self) {
        self.history.begin(self.cursor.position());
        let line = self.cursor.line();
        let sol = Position::new(line, 0);
        self.buffer.insert(sol, "\n");
        self.history.record_insert(sol, "\n");
        self.cursor
            .set_position(Position::new(line, 0), &self.buffer, true);
        self.mode = Mode::Insert;
    }

    // ── Edit commands ────────────────────────────────────────────────────

    /// Delete the character under the cursor (`x` in Vim).
    fn delete_char_at_cursor(&mut self) {
        let pe = self.mode.cursor_past_end();
        let pos = self.cursor.position();
        let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);

        if line_len == 0 || pos.col >= line_len {
            return;
        }

        let ch = self.buffer.char_at(pos).unwrap();
        let to = Position::new(pos.line, pos.col + 1);

        self.history.begin(pos);
        self.history.record_delete(pos, &ch.to_string());
        self.buffer.delete(Range::new(pos, to));
        self.cursor.clamp(&self.buffer, pe);
        self.history.commit(self.cursor.position());
    }

    /// Delete the current line (`dd` in Vim).
    fn delete_current_line(&mut self) {
        let pe = self.mode.cursor_past_end();
        let line = self.cursor.line();
        let line_count = self.buffer.line_count();

        if self.buffer.is_empty() {
            return;
        }

        // Determine the range to delete.
        let (from, to) = if line_count == 1 {
            // Only line — clear everything.
            let len = self.buffer.line_len(0).unwrap_or(0);
            if len == 0 {
                return;
            }
            (Position::ZERO, Position::new(0, len))
        } else if line + 1 < line_count {
            // Not the last line — delete through the trailing newline.
            (Position::new(line, 0), Position::new(line + 1, 0))
        } else {
            // Last line — also remove the preceding newline so we don't
            // leave a trailing blank line.
            let prev_len = self.buffer.line_content_len(line - 1).unwrap_or(0);
            let this_len = self.buffer.line_len(line).unwrap_or(0);
            (
                Position::new(line - 1, prev_len),
                Position::new(line, this_len),
            )
        };

        let range = Range::new(from, to);
        let deleted = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        self.history.begin(self.cursor.position());
        self.history.record_delete(from, &deleted);
        self.buffer.delete(range);
        self.cursor.clamp(&self.buffer, pe);
        self.cursor.move_to_first_non_blank(&self.buffer, pe);
        self.history.commit(self.cursor.position());
    }
}

// ─── App implementation ─────────────────────────────────────────────────────

impl App for Editor {
    fn on_event(&mut self, event: &Event) -> Action {
        let Event::Key(key) = event else {
            return Action::Continue;
        };

        // Only handle key presses, not releases or repeats (for now).
        if key.kind != n_term::input::KeyEventKind::Press {
            return Action::Continue;
        }

        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            // Visual, Replace — not yet implemented.
            _ => Action::Continue,
        }
    }

    fn on_resize(&mut self, _size: Size) {
        // The event loop already resized the framebuffer. The view will
        // adjust scroll on the next paint via ensure_cursor_visible.
    }

    fn paint(&mut self, frame: &mut FrameBuffer) {
        let w = frame.width();
        let h = frame.height();

        if h < 2 {
            // Too small for text + status + command line. Just render
            // what we can into the View.
            self.cursor_screen = self.view.render(
                &self.buffer,
                &self.cursor,
                self.mode,
                frame,
                0,
                0,
                w,
                h,
            );
            return;
        }

        // Give the View all rows except the bottom one (command/message line).
        let view_height = h - 1;
        self.cursor_screen = self.view.render(
            &self.buffer,
            &self.cursor,
            self.mode,
            frame,
            0,
            0,
            w,
            view_height,
        );

        // Bottom row: command line or message.
        let bottom_y = h - 1;

        if self.mode == Mode::Command {
            // Render the command line and position the cursor there.
            let cmd_cursor = view::render_command_line(
                frame,
                self.cmdline.input(),
                self.cmdline.cursor(),
                0,
                bottom_y,
                w,
            );
            // In command mode, the cursor lives on the command line.
            self.cursor_screen = cmd_cursor;
        } else if let Some(ref msg) = self.message {
            view::render_message_line(frame, msg, self.message_is_error, 0, bottom_y, w);
        } else {
            // Empty bottom line.
            view::render_message_line(frame, "", false, 0, bottom_y, w);
        }
    }

    fn cursor(&self) -> Option<(u16, u16, CursorShape)> {
        let (x, y) = self.cursor_screen?;

        let shape = match self.mode.cursor_shape() {
            n_editor::mode::CursorShape::SteadyBlock => CursorShape::SteadyBlock,
            n_editor::mode::CursorShape::SteadyBar => CursorShape::SteadyBar,
            n_editor::mode::CursorShape::SteadyUnderline => CursorShape::SteadyUnderline,
        };

        Some((x, y, shape))
    }
}

// ─── Entry point ────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut editor = if args.len() > 1 {
        Editor::from_file(&args[1])
    } else {
        Editor::new()
    };

    let mut event_loop = EventLoop::new().unwrap_or_else(|e| {
        eprintln!("n-nvim: failed to initialize terminal: {e}");
        process::exit(1);
    });

    if let Err(e) = event_loop.run(&mut editor) {
        eprintln!("n-nvim: {e}");
        process::exit(1);
    }
}
