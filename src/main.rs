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
// This is the moment n-nvim stops being a library and starts being a
// real editor you can see and use.

use std::env;
use std::path::PathBuf;
use std::process;

use n_editor::buffer::Buffer;
use n_editor::cursor::Cursor;
use n_editor::mode::Mode;
use n_editor::position::Position;
use n_editor::view::View;

use n_term::ansi::CursorShape;
use n_term::buffer::FrameBuffer;
use n_term::event_loop::{Action, App, EventLoop};
use n_term::input::{Event, KeyCode, KeyEvent, Modifiers};
use n_term::terminal::Size;

// ─── Editor ─────────────────────────────────────────────────────────────────

/// The editor application state.
///
/// Holds everything needed to edit a file: the text buffer, cursor position,
/// current mode, view configuration, and the screen position of the cursor
/// computed during the last paint.
struct Editor {
    buffer: Buffer,
    cursor: Cursor,
    view: View,
    mode: Mode,

    /// Screen position of the cursor from the last paint, used by the
    /// event loop to position the hardware terminal cursor.
    cursor_screen: Option<(u16, u16)>,
}

impl Editor {
    /// Create an editor with an empty buffer.
    fn new() -> Self {
        Self {
            buffer: Buffer::new(),
            cursor: Cursor::new(),
            view: View::new(),
            mode: Mode::Normal,
            cursor_screen: None,
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
            cursor_screen: None,
        }
    }

    // ── Mode dispatch ───────────────────────────────────────────────────

    fn handle_normal(&mut self, key: &KeyEvent) -> Action {
        let pe = self.mode.cursor_past_end();

        // Ctrl+C always quits, regardless of mode.
        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        match key.code {
            // -- Quit --
            KeyCode::Char('q') => return Action::Quit,

            // -- Mode transitions --
            KeyCode::Char('i') => {
                self.mode = Mode::Insert;
            }
            KeyCode::Char('a') => {
                // Append: move right one, enter insert.
                self.cursor.move_right(1, &self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                // Append at end of line.
                self.cursor.move_to_line_end(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                // Insert at first non-blank.
                self.cursor.move_to_first_non_blank(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                // Open line below.
                let line = self.cursor.line();
                let line_len = self.buffer.line_content_len(line).unwrap_or(0);
                let eol = Position::new(line, line_len);
                self.buffer.insert(eol, "\n");
                self.cursor
                    .set_position(Position::new(line + 1, 0), &self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('O') => {
                // Open line above.
                let line = self.cursor.line();
                let sol = Position::new(line, 0);
                self.buffer.insert(sol, "\n");
                self.cursor
                    .set_position(Position::new(line, 0), &self.buffer, true);
                self.mode = Mode::Insert;
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

    fn handle_insert(&mut self, key: &KeyEvent) -> Action {
        // Ctrl+C quits from any mode.
        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        match key.code {
            KeyCode::Escape => {
                self.mode = Mode::Normal;
                // Vim moves cursor back one position when leaving insert mode.
                self.cursor.move_left(1, &self.buffer, false);
            }

            KeyCode::Char(ch) => {
                let pos = self.cursor.position();
                self.buffer.insert_char(pos, ch);
                self.cursor.move_right(1, &self.buffer, true);
            }

            KeyCode::Enter => {
                let pos = self.cursor.position();
                self.buffer.insert_char(pos, '\n');
                self.cursor
                    .set_position(Position::new(pos.line + 1, 0), &self.buffer, true);
            }

            KeyCode::Backspace => {
                let pos = self.cursor.position();
                if pos.col > 0 {
                    // Delete the character before the cursor, land where it was.
                    let from = Position::new(pos.line, pos.col - 1);
                    self.buffer.delete(n_editor::position::Range::new(from, pos));
                    self.cursor.set_position(from, &self.buffer, true);
                } else if pos.line > 0 {
                    // At column 0: join with previous line.
                    let prev_line = pos.line - 1;
                    let prev_len = self.buffer.line_content_len(prev_line).unwrap_or(0);
                    let from = Position::new(prev_line, prev_len);
                    self.buffer.delete(n_editor::position::Range::new(from, pos));
                    self.cursor.set_position(from, &self.buffer, true);
                }
            }

            KeyCode::Delete => {
                let pos = self.cursor.position();
                let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);
                if pos.col < line_len {
                    let to = Position::new(pos.line, pos.col + 1);
                    self.buffer
                        .delete(n_editor::position::Range::new(pos, to));
                } else if pos.line + 1 < self.buffer.line_count() {
                    // At end of line: join with next line.
                    let to = Position::new(pos.line + 1, 0);
                    self.buffer
                        .delete(n_editor::position::Range::new(pos, to));
                }
            }

            // Arrow keys work in insert mode too.
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
            // Visual, Replace, Command — not yet implemented.
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
    }

    fn cursor(&self) -> Option<(u16, u16, CursorShape)> {
        let (x, y) = self.cursor_screen?;

        // Map editor's mode-based cursor shape to the terminal's shape enum.
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
