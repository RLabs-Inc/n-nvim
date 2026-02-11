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
use n_editor::mode::{Mode, VisualKind};
use n_editor::position::{Position, Range};
use n_editor::register::{Register, RegisterKind};
use n_editor::search::{self, SearchDirection, SearchState};
use n_editor::text_object;
use n_editor::view::{self, View};

use n_term::ansi::CursorShape;
use n_term::buffer::FrameBuffer;
use n_term::event_loop::{Action, App, EventLoop};
use n_term::input::{Event, KeyCode, KeyEvent, Modifiers};
use n_term::terminal::Size;

// ─── Character find direction ───────────────────────────────────────────────

/// Direction and mode for `f`/`F`/`t`/`T` character-find motions.
///
/// Stored in `Editor::last_char_find` so `;` and `,` can repeat the last find.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CharFindKind {
    /// `f` — find char forward, land on it.
    Forward,
    /// `F` — find char backward, land on it.
    Backward,
    /// `t` — find char forward, land one before it.
    TillForward,
    /// `T` — find char backward, land one after it.
    TillBackward,
}

impl CharFindKind {
    /// Return the opposite direction (for `,` repeat).
    const fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::TillForward => Self::TillBackward,
            Self::TillBackward => Self::TillForward,
        }
    }
}

// ─── Pending state ──────────────────────────────────────────────────────────

/// Multi-key command state for operator-pending mode.
///
/// Vim's grammar: `[count] operator [count] [motion | text-object]`.
/// After pressing an operator key (`d`, `c`, `y`), we enter operator-pending
/// mode and wait for:
///
/// - The same key again → line operation (`dd`, `yy`, `cc`)
/// - A motion key → operate from cursor to motion target (`dw`, `d$`, `cw`)
/// - `i`/`a` + object key → operate on a text object (`diw`, `ci"`, `ya(`)
/// - `f`/`F`/`t`/`T` + char → operate to the character find target (`dfa`)
///
/// The `count` field stores the operator's count (typed before the operator).
/// A second count can be typed before the motion; the effective count is
/// `op_count * motion_count` (e.g., `2d3w` deletes 6 words).
#[derive(Clone, Copy)]
enum Pending {
    /// Operator pressed (`d`, `c`, `y`). Waiting for motion, text-object
    /// prefix, or the same key for a line operation.
    Operator { op: char, count: usize },
    /// Operator + text-object prefix (`di`, `ca`, `yi`). Waiting for the
    /// object key (`w`, `"`, `(`, etc.).
    TextObject { op: char, inner: bool, count: usize },
    /// Standalone char find (`f`, `F`, `t`, `T`). Waiting for the target char.
    CharFind { kind: CharFindKind, count: usize },
    /// Operator + char find (`df`, `ct`, etc.). Waiting for the target char.
    OperatorCharFind {
        op: char,
        op_count: usize,
        kind: CharFindKind,
        motion_count: usize,
    },
}

// ─── Dot-repeat ─────────────────────────────────────────────────────────────

/// Recorded state of the last buffer-modifying change, for `.` (dot-repeat).
///
/// The key sequence has all digit keys stripped out — counts are tracked
/// separately so that a count before `.` can override the original.
///
/// Examples:
///
///   `2d3w`        → count=Some(6), keys=[d, w]
///   `dw`          → count=None,    keys=[d, w]
///   `x`           → count=None,    keys=[x]
///   `ihello<Esc>` → count=None,    keys=[i, h, e, l, l, o, Esc]
#[derive(Clone)]
struct DotRepeat {
    /// The effective count for the change. `None` means no explicit count.
    count: Option<usize>,
    /// Key sequence with all count digits removed.
    keys: Vec<KeyEvent>,
}

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

    /// Multi-key command state. When an operator key (`d`, `c`, `y`) is
    /// pressed, this tracks the pending state until the command is completed
    /// or cancelled.
    pending: Option<Pending>,

    /// Numeric count accumulator. Built from digit keystrokes (1-9 start,
    /// then 0-9 extend). `None` means no count entered. Consumed by the
    /// next motion, operator, or command — `take_count()` returns the value
    /// and resets to `None`.
    count: Option<usize>,

    /// Screen position of the cursor from the last paint, used by the
    /// event loop to position the hardware terminal cursor.
    cursor_screen: Option<(u16, u16)>,

    /// The command-line input buffer (active when mode == Command).
    cmdline: CommandLine,

    /// The unnamed register — stores yanked and deleted text for `p`/`P`.
    register: Register,

    /// A message to display on the bottom line. Cleared on the next keypress.
    message: Option<String>,

    /// Whether the current message is an error (renders in red).
    message_is_error: bool,

    /// Active search-input session. When `Some`, the editor is accepting
    /// search input on the bottom line (incremental search mode).
    search: Option<SearchState>,

    /// Last confirmed search pattern. Persists across searches for `n`/`N`
    /// repeat. Empty string means no previous search.
    last_search: String,

    /// Direction of the last search. Used by `n` (same direction) and `N`
    /// (opposite direction).
    last_search_direction: SearchDirection,

    /// Last character find for `;` and `,` repeat. Stores the target char
    /// and the kind (f/F/t/T) of the most recent character find.
    last_char_find: Option<(char, CharFindKind)>,

    /// True when recording keys for dot-repeat. Insert-mode keys are
    /// recorded verbatim; normal-mode digit keys are excluded (counts
    /// are tracked separately in `dot_effective_count`).
    dot_recording: bool,

    /// Accumulated key sequence for the change being recorded.
    dot_keys: Vec<KeyEvent>,

    /// Effective count for the change being recorded (`op_count × motion_count`).
    dot_effective_count: Option<usize>,

    /// The last completed change, ready for `.` replay.
    last_change: Option<DotRepeat>,

    /// True during `.` replay — suppresses recording so replayed keys
    /// don't overwrite `last_change`.
    dot_replaying: bool,
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
            pending: None,
            count: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            register: Register::new(),
            message: None,
            message_is_error: false,
            search: None,
            last_search: String::new(),
            last_search_direction: SearchDirection::Forward,
            last_char_find: None,
            dot_recording: false,
            dot_keys: Vec::new(),
            dot_effective_count: None,
            last_change: None,
            dot_replaying: false,
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
            pending: None,
            count: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            register: Register::new(),
            message: None,
            message_is_error: false,
            search: None,
            last_search: String::new(),
            last_search_direction: SearchDirection::Forward,
            last_char_find: None,
            dot_recording: false,
            dot_keys: Vec::new(),
            dot_effective_count: None,
            last_change: None,
            dot_replaying: false,
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

    // ── Count accumulation ─────────────────────────────────────────────

    /// Take the accumulated count and reset. Returns `None` if no count was
    /// entered, `Some(n)` if digits were pressed.
    const fn take_raw_count(&mut self) -> Option<usize> {
        self.count.take()
    }

    /// Take the accumulated count, defaulting to 1. Use when the count is
    /// simply a repeat multiplier.
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    /// Push a digit onto the count accumulator (0-9).
    fn push_count_digit(&mut self, digit: u8) {
        let current = self.count.unwrap_or(0);
        // Cap at a reasonable maximum to prevent overflow from mashing digits.
        self.count = Some(current.saturating_mul(10).saturating_add(digit as usize));
    }

    // ── Dot-repeat recording ────────────────────────────────────────────

    /// Merge two optional counts by multiplication.
    ///
    /// Returns `None` only when both inputs are `None` (no count typed).
    const fn merge_counts(a: Option<usize>, b: Option<usize>) -> Option<usize> {
        match (a, b) {
            (None, None) => None,
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (Some(x), Some(y)) => Some(x.saturating_mul(y)),
        }
    }

    /// Start recording a change for dot-repeat.
    ///
    /// Saves the initiating key and the raw count. Subsequent keys are
    /// recorded by [`handle_pending`] and [`handle_insert`].
    fn dot_start(&mut self, key: &KeyEvent, raw_count: Option<usize>) {
        if self.dot_replaying {
            return;
        }
        self.dot_recording = true;
        self.dot_keys.clear();
        self.dot_keys.push(*key);
        self.dot_effective_count = raw_count;
    }

    /// Record a single-key change and finalize immediately.
    ///
    /// Used for commands like `x`, `p`, `P` that complete in one key.
    fn dot_immediate(&mut self, key: &KeyEvent, raw_count: Option<usize>) {
        if self.dot_replaying {
            return;
        }
        self.dot_recording = false;
        self.last_change = Some(DotRepeat {
            count: raw_count,
            keys: vec![*key],
        });
    }

    /// Finalize the current dot-repeat recording.
    fn dot_finish(&mut self) {
        if self.dot_replaying {
            return;
        }
        self.dot_recording = false;
        if self.dot_keys.is_empty() {
            return;
        }
        self.last_change = Some(DotRepeat {
            count: self.dot_effective_count,
            keys: self.dot_keys.clone(),
        });
    }

    /// Cancel an in-progress dot recording without saving.
    fn dot_cancel(&mut self) {
        self.dot_recording = false;
        self.dot_keys.clear();
    }

    /// Replay the last change (`.` command).
    ///
    /// If `count_override` is `Some`, it replaces the stored count.
    /// Otherwise the original effective count is reused.
    fn dot_replay(&mut self, count_override: Option<usize>) -> Action {
        let Some(change) = self.last_change.clone() else {
            return Action::Continue;
        };

        let effective = count_override.or(change.count);
        self.dot_replaying = true;
        self.count = effective;

        for key in &change.keys {
            let event = Event::Key(*key);
            let action = self.on_event(&event);
            if matches!(action, Action::Quit) {
                self.dot_replaying = false;
                return Action::Quit;
            }
        }

        self.dot_replaying = false;
        Action::Continue
    }

    // ── Shared motion dispatch ──────────────────────────────────────────

    /// Apply a cursor motion from the given key. Returns `true` if the key
    /// was consumed as a motion, `false` if it wasn't a recognized motion.
    ///
    /// `raw_count` is `None` when no digits were pressed, `Some(n)` otherwise.
    /// Most motions use the count as a repeat multiplier (default 1), but
    /// `G` and `g` treat it as a 1-indexed line number.
    ///
    /// This is shared between normal and visual modes so both can move the
    /// cursor with the same keys without duplicating the dispatch table.
    fn apply_motion(&mut self, code: KeyCode, pe: bool, raw_count: Option<usize>) -> bool {
        let count = raw_count.unwrap_or(1);
        match code {
            // Basic movement
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor.move_left(count, &self.buffer, pe);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor.move_right(count, &self.buffer, pe);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.move_down(count, &self.buffer, pe);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.move_up(count, &self.buffer, pe);
            }

            // Line motions (count doesn't affect these)
            KeyCode::Char('0') | KeyCode::Home => {
                self.cursor.move_to_line_start();
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.cursor.move_to_line_end(&self.buffer, pe);
            }
            KeyCode::Char('^') => {
                self.cursor.move_to_first_non_blank(&self.buffer, pe);
            }

            // Word motions
            KeyCode::Char('w') => self.cursor.word_forward(count, &self.buffer, pe),
            KeyCode::Char('b') => self.cursor.word_backward(count, &self.buffer, pe),
            KeyCode::Char('e') => self.cursor.word_end_forward(count, &self.buffer, pe),
            KeyCode::Char('W') => self.cursor.big_word_forward(count, &self.buffer, pe),
            KeyCode::Char('B') => self.cursor.big_word_backward(count, &self.buffer, pe),
            KeyCode::Char('E') => self.cursor.big_word_end_forward(count, &self.buffer, pe),

            // File motions: count = line number (1-indexed), no count = first/last
            KeyCode::Char('g') => {
                if let Some(n) = raw_count {
                    self.cursor.goto_line(n.saturating_sub(1), &self.buffer, pe);
                } else {
                    self.cursor.move_to_first_line(&self.buffer, pe);
                }
            }
            KeyCode::Char('G') => {
                if let Some(n) = raw_count {
                    self.cursor.goto_line(n.saturating_sub(1), &self.buffer, pe);
                } else {
                    self.cursor.move_to_last_line(&self.buffer, pe);
                }
            }

            // Character find repeat (single-key motions — no pending needed).
            KeyCode::Char(';') => {
                if let Some((ch, kind)) = self.last_char_find {
                    self.execute_char_find_motion(ch, kind, count, pe);
                }
            }
            KeyCode::Char(',') => {
                if let Some((ch, kind)) = self.last_char_find {
                    self.execute_char_find_motion(ch, kind.opposite(), count, pe);
                }
            }

            _ => return false,
        }
        true
    }

    /// Execute a character-find motion (used by f/F/t/T and ;/,).
    fn execute_char_find_motion(
        &mut self,
        ch: char,
        kind: CharFindKind,
        count: usize,
        pe: bool,
    ) {
        match kind {
            CharFindKind::Forward => {
                self.cursor.char_find_forward(&self.buffer, ch, count, pe);
            }
            CharFindKind::Backward => {
                self.cursor.char_find_backward(&self.buffer, ch, count, pe);
            }
            CharFindKind::TillForward => {
                self.cursor.char_till_forward(&self.buffer, ch, count, pe);
            }
            CharFindKind::TillBackward => {
                self.cursor.char_till_backward(&self.buffer, ch, count, pe);
            }
        }
    }

    /// Compute the operator range for a character-find motion.
    ///
    /// All character-find motions (f/F/t/T) are inclusive — the character at
    /// the end of the range is included in the operation.
    fn char_find_operator_range(
        &self,
        ch: char,
        kind: CharFindKind,
        count: usize,
    ) -> Option<Range> {
        let start = self.cursor.position();
        let mut c = self.cursor.clone();

        let moved = match kind {
            CharFindKind::Forward => c.char_find_forward(&self.buffer, ch, count, false),
            CharFindKind::Backward => c.char_find_backward(&self.buffer, ch, count, false),
            CharFindKind::TillForward => c.char_till_forward(&self.buffer, ch, count, false),
            CharFindKind::TillBackward => c.char_till_backward(&self.buffer, ch, count, false),
        };

        if !moved {
            return None;
        }

        let end = c.position();
        if start == end {
            return None;
        }

        // Inclusive: extend to include the character at the far end.
        let (from, to) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        let end_line_len = self.buffer.line_content_len(to.line).unwrap_or(0);
        let extended = if to.col < end_line_len {
            Position::new(to.line, to.col + 1)
        } else {
            Position::new(to.line, end_line_len)
        };

        Some(Range::new(from, extended))
    }

    // ── Normal mode ──────────────────────────────────────────────────────

    fn handle_normal(&mut self, key: &KeyEvent) -> Action {
        // Any keypress in normal mode clears the message line.
        self.clear_message();

        let pe = self.mode.cursor_past_end();

        // Ctrl combinations cancel pending state and consume the count.
        if key.modifiers.contains(Modifiers::CTRL) {
            match key.code {
                KeyCode::Char('c') => return Action::Quit,
                KeyCode::Char('r') => {
                    self.pending = None;
                    let count = self.take_count();
                    let mut last_pos = None;
                    for _ in 0..count {
                        if let Some(pos) = self.history.redo(&mut self.buffer) {
                            last_pos = Some(pos);
                        } else {
                            break;
                        }
                    }
                    if let Some(pos) = last_pos {
                        self.cursor.set_position(pos, &self.buffer, pe);
                    }
                    return Action::Continue;
                }
                KeyCode::Char('v') => {
                    self.pending = None;
                    self.count = None;
                    self.cursor.set_anchor();
                    self.mode = Mode::Visual(VisualKind::Block);
                    return Action::Continue;
                }
                _ => {}
            }
        }

        // Handle pending operator state (multi-key commands).
        // Digits can be pressed between operator and motion (e.g., `d3w`),
        // so digit accumulation also happens inside handle_pending.
        if let Some(pending) = self.pending.take() {
            return self.handle_pending(pending, key);
        }

        // Digit accumulation for counts.
        // 1-9 start a new count. 0 extends an existing count but is
        // move-to-line-start when no count is being built.
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.push_count_digit(d as u8 - b'0');
                return Action::Continue;
            }
            KeyCode::Char('0') if self.count.is_some() => {
                self.push_count_digit(0);
                return Action::Continue;
            }
            _ => {}
        }

        // Take the accumulated count for the command that follows.
        let raw_count = self.take_raw_count();
        self.handle_normal_key(key, pe, raw_count)
    }

    /// Dispatch a keypress in operator-pending mode.
    ///
    /// After an operator (`d`, `c`, `y`) is pressed, the next key(s) determine
    /// what to operate on: a motion, a text object, or the same key for a line
    /// operation.
    ///
    /// Digits can appear between operator and motion (e.g., `d3w`). These build
    /// a "motion count" that multiplies with the operator's count.
    #[allow(clippy::too_many_lines)]
    fn handle_pending(&mut self, pending: Pending, key: &KeyEvent) -> Action {
        match pending {
            Pending::Operator { op, count: op_count } => {
                // Escape cancels the pending operator and any motion count.
                if key.code == KeyCode::Escape {
                    self.count = None;
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Digit accumulation for motion count (e.g., the `3` in `d3w`).
                // Digits are NOT recorded — they're folded into dot_effective_count.
                match key.code {
                    KeyCode::Char(d @ '1'..='9') => {
                        self.push_count_digit(d as u8 - b'0');
                        self.pending = Some(Pending::Operator { op, count: op_count });
                        return Action::Continue;
                    }
                    KeyCode::Char('0') if self.count.is_some() => {
                        self.push_count_digit(0);
                        self.pending = Some(Pending::Operator { op, count: op_count });
                        return Action::Continue;
                    }
                    _ => {}
                }

                // Record this key for dot-repeat (non-digit, non-escape).
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                // Same key = line operation (dd, yy, cc).
                // Effective count: op_count * motion_count.
                if key.code == KeyCode::Char(op) {
                    let raw_motion_count = self.take_raw_count();
                    let motion_count = raw_motion_count.unwrap_or(1);
                    let effective = op_count * motion_count;

                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }

                    let action = self.operator_line(op, effective);

                    // Finalize unless the operator entered insert mode (cc).
                    if self.dot_recording && !self.dot_replaying && self.mode != Mode::Insert
                    {
                        self.dot_finish();
                    }

                    return action;
                }

                // Text object prefix: i = inner, a = around.
                // The operator count carries forward. Recording continues.
                if key.code == KeyCode::Char('i') {
                    self.pending = Some(Pending::TextObject {
                        op,
                        inner: true,
                        count: op_count,
                    });
                    return Action::Continue;
                }
                if key.code == KeyCode::Char('a') {
                    self.pending = Some(Pending::TextObject {
                        op,
                        inner: false,
                        count: op_count,
                    });
                    return Action::Continue;
                }

                // Character find prefix: f/F/t/T need one more key.
                let char_find_kind = match key.code {
                    KeyCode::Char('f') => Some(CharFindKind::Forward),
                    KeyCode::Char('F') => Some(CharFindKind::Backward),
                    KeyCode::Char('t') => Some(CharFindKind::TillForward),
                    KeyCode::Char('T') => Some(CharFindKind::TillBackward),
                    _ => None,
                };
                if let Some(kind) = char_find_kind {
                    let raw_motion_count = self.take_raw_count();
                    let motion_count = raw_motion_count.unwrap_or(1);
                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }
                    self.pending = Some(Pending::OperatorCharFind {
                        op,
                        op_count,
                        kind,
                        motion_count,
                    });
                    return Action::Continue;
                }

                // `;`/`,` repeat the last character find as an operator motion.
                if key.code == KeyCode::Char(';') || key.code == KeyCode::Char(',') {
                    if let Some((ch, stored_kind)) = self.last_char_find {
                        let kind = if key.code == KeyCode::Char(',') {
                            stored_kind.opposite()
                        } else {
                            stored_kind
                        };
                        let raw_motion_count = self.take_raw_count();
                        let effective = op_count * raw_motion_count.unwrap_or(1);
                        if self.dot_recording && !self.dot_replaying {
                            self.dot_effective_count =
                                Self::merge_counts(self.dot_effective_count, raw_motion_count);
                        }
                        if let Some(range) = self.char_find_operator_range(ch, kind, effective) {
                            let action = self.apply_operator(op, range, false);
                            if self.dot_recording
                                && !self.dot_replaying
                                && self.mode != Mode::Insert
                            {
                                self.dot_finish();
                            }
                            return action;
                        }
                    }
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Try as a motion. The motion's own count multiplies with
                // the operator count, except for g/G where it's a line number.
                let raw_motion_count = self.take_raw_count();
                let effective = op_count * raw_motion_count.unwrap_or(1);
                if let Some(range) =
                    self.operator_motion_range(key.code, op, effective, raw_motion_count)
                {
                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }

                    let action = self.apply_operator(op, range, false);

                    if self.dot_recording && !self.dot_replaying && self.mode != Mode::Insert
                    {
                        self.dot_finish();
                    }

                    return action;
                }

                // Unrecognized key — cancel the operator silently.
                self.dot_cancel();
                Action::Continue
            }
            Pending::TextObject { op, inner, count: _op_count } => {
                // Escape cancels.
                if key.code == KeyCode::Escape {
                    self.count = None;
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Record the text object key for dot-repeat.
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                if let Some(range) = self.text_object_range(key.code, inner) {
                    let action = self.apply_operator(op, range, false);

                    if self.dot_recording && !self.dot_replaying && self.mode != Mode::Insert
                    {
                        self.dot_finish();
                    }

                    return action;
                }

                // Unrecognized text object key — cancel silently.
                self.dot_cancel();
                Action::Continue
            }
            Pending::CharFind { kind, count } => {
                // Standalone f/F/t/T: waiting for the target character.
                if key.code == KeyCode::Escape {
                    return Action::Continue;
                }
                if let KeyCode::Char(ch) = key.code {
                    self.last_char_find = Some((ch, kind));
                    let pe = self.mode.cursor_past_end();
                    self.execute_char_find_motion(ch, kind, count, pe);
                }
                Action::Continue
            }
            Pending::OperatorCharFind {
                op,
                op_count,
                kind,
                motion_count,
            } => {
                // Operator + f/F/t/T: waiting for the target character.
                if key.code == KeyCode::Escape {
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Record the target char for dot-repeat.
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                if let KeyCode::Char(ch) = key.code {
                    self.last_char_find = Some((ch, kind));
                    let effective = op_count * motion_count;
                    if let Some(range) = self.char_find_operator_range(ch, kind, effective) {
                        let action = self.apply_operator(op, range, false);
                        if self.dot_recording
                            && !self.dot_replaying
                            && self.mode != Mode::Insert
                        {
                            self.dot_finish();
                        }
                        return action;
                    }
                }

                self.dot_cancel();
                Action::Continue
            }
        }
    }

    /// Process a single normal-mode key (no pending operator).
    ///
    /// `raw_count` is the accumulated numeric prefix — `None` if no digits
    /// were pressed, `Some(n)` otherwise.
    #[allow(clippy::too_many_lines)]
    fn handle_normal_key(
        &mut self,
        key: &KeyEvent,
        pe: bool,
        raw_count: Option<usize>,
    ) -> Action {
        let count = raw_count.unwrap_or(1);

        // Try motion keys first (shared with visual mode).
        if self.apply_motion(key.code, pe, raw_count) {
            return Action::Continue;
        }

        match key.code {
            // -- Enter command mode --
            KeyCode::Char(':') => {
                self.cmdline.clear();
                self.mode = Mode::Command;
            }

            // -- Enter visual mode --
            KeyCode::Char('v') => {
                self.cursor.set_anchor();
                self.mode = Mode::Visual(VisualKind::Char);
            }
            KeyCode::Char('V') => {
                self.cursor.set_anchor();
                self.mode = Mode::Visual(VisualKind::Line);
            }

            // -- Mode transitions (all begin a history transaction) --
            KeyCode::Char('i') => {
                self.dot_start(key, raw_count);
                self.history.begin(self.cursor.position());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('a') => {
                self.dot_start(key, raw_count);
                self.history.begin(self.cursor.position());
                self.cursor.move_right(1, &self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                self.dot_start(key, raw_count);
                self.history.begin(self.cursor.position());
                self.cursor.move_to_line_end(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                self.dot_start(key, raw_count);
                self.history.begin(self.cursor.position());
                self.cursor.move_to_first_non_blank(&self.buffer, true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                self.dot_start(key, raw_count);
                self.open_line_below();
            }
            KeyCode::Char('O') => {
                self.dot_start(key, raw_count);
                self.open_line_above();
            }

            // -- Operators (enter pending mode with count) --
            KeyCode::Char('d') => {
                self.dot_start(key, raw_count);
                self.pending = Some(Pending::Operator { op: 'd', count });
            }
            KeyCode::Char('c') => {
                self.dot_start(key, raw_count);
                self.pending = Some(Pending::Operator { op: 'c', count });
            }
            KeyCode::Char('y') => {
                // Yank is not a buffer change — don't record for dot-repeat.
                self.pending = Some(Pending::Operator { op: 'y', count });
            }
            KeyCode::Char('x') => {
                self.dot_immediate(key, raw_count);
                self.delete_chars_at_cursor(count);
            }

            // -- Yank line shortcut (not a change) --
            KeyCode::Char('Y') => {
                self.operator_line('y', count);
            }

            // -- Paste --
            KeyCode::Char('p') => {
                self.dot_immediate(key, raw_count);
                self.paste_after(count);
            }
            KeyCode::Char('P') => {
                self.dot_immediate(key, raw_count);
                self.paste_before(count);
            }

            // -- Dot-repeat --
            KeyCode::Char('.') => {
                return self.dot_replay(raw_count);
            }

            // -- Undo --
            KeyCode::Char('u') => {
                let mut last_pos = None;
                for _ in 0..count {
                    if let Some(pos) = self.history.undo(&mut self.buffer) {
                        last_pos = Some(pos);
                    } else {
                        break;
                    }
                }
                if let Some(pos) = last_pos {
                    self.cursor.set_position(pos, &self.buffer, pe);
                }
            }

            // -- Character find (enter pending, waiting for target char) --
            KeyCode::Char('f') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::Forward,
                    count,
                });
            }
            KeyCode::Char('F') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::Backward,
                    count,
                });
            }
            KeyCode::Char('t') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::TillForward,
                    count,
                });
            }
            KeyCode::Char('T') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::TillBackward,
                    count,
                });
            }

            // -- Search --
            KeyCode::Char('/') => self.start_search(SearchDirection::Forward),
            KeyCode::Char('?') => self.start_search(SearchDirection::Backward),
            KeyCode::Char('n') => {
                for _ in 0..count {
                    self.search_next();
                }
            }
            KeyCode::Char('N') => {
                for _ in 0..count {
                    self.search_prev();
                }
            }
            KeyCode::Char('*') => {
                self.search_word_under_cursor(SearchDirection::Forward);
            }
            KeyCode::Char('#') => {
                self.search_word_under_cursor(SearchDirection::Backward);
            }

            _ => {}
        }

        Action::Continue
    }

    // ── Operator dispatch ───────────────────────────────────────────────

    /// Compute the motion range for an operator + motion combination.
    ///
    /// Uses a temporary cursor clone to compute where the motion would go,
    /// then builds a half-open range. Handles exclusive/inclusive motion types
    /// and linewise motions.
    ///
    /// `effective` is the pre-multiplied count (`op_count * motion_count`) for
    /// most motions. `raw_motion_count` preserves whether the user typed a
    /// motion count, needed by `G`/`g` where the count is a line number.
    #[allow(clippy::too_many_lines)]
    fn operator_motion_range(
        &self,
        code: KeyCode,
        op: char,
        effective: usize,
        raw_motion_count: Option<usize>,
    ) -> Option<Range> {
        let start = self.cursor.position();
        let mut c = self.cursor.clone();

        // Returns true for inclusive motions (range must extend past target).
        let inclusive = match code {
            // Exclusive motions — range end IS the target position.
            KeyCode::Char('h') | KeyCode::Left => {
                c.move_left(effective, &self.buffer, false);
                false
            }
            KeyCode::Char('l') | KeyCode::Right => {
                c.move_right(effective, &self.buffer, false);
                false
            }
            KeyCode::Char('0') | KeyCode::Home => {
                c.move_to_line_start();
                false
            }
            KeyCode::Char('^') => {
                c.move_to_first_non_blank(&self.buffer, false);
                false
            }
            KeyCode::Char('b') => {
                c.word_backward(effective, &self.buffer, false);
                false
            }
            KeyCode::Char('B') => {
                c.big_word_backward(effective, &self.buffer, false);
                false
            }

            // Special case: cw/cW act like ce/cE (Vim compatibility).
            KeyCode::Char('w') if op == 'c' => {
                c.word_end_forward(effective, &self.buffer, false);
                true
            }
            KeyCode::Char('W') if op == 'c' => {
                c.big_word_end_forward(effective, &self.buffer, false);
                true
            }

            KeyCode::Char('w') => {
                c.word_forward(effective, &self.buffer, false);
                false
            }
            KeyCode::Char('W') => {
                c.big_word_forward(effective, &self.buffer, false);
                false
            }

            // Inclusive motions — range extends to include the target char.
            KeyCode::Char('e') => {
                c.word_end_forward(effective, &self.buffer, false);
                true
            }
            KeyCode::Char('E') => {
                c.big_word_end_forward(effective, &self.buffer, false);
                true
            }
            KeyCode::Char('$') | KeyCode::End => {
                c.move_to_line_end(&self.buffer, false);
                true
            }

            // Linewise motions — expand to full lines.
            KeyCode::Char('j') | KeyCode::Down => {
                c.move_down(effective, &self.buffer, false);
                return self.linewise_range(start, c.position());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                c.move_up(effective, &self.buffer, false);
                return self.linewise_range(start, c.position());
            }
            KeyCode::Char('G') => {
                if let Some(n) = raw_motion_count {
                    c.goto_line(n.saturating_sub(1), &self.buffer, false);
                } else {
                    c.move_to_last_line(&self.buffer, false);
                }
                return self.linewise_range(start, c.position());
            }
            KeyCode::Char('g') => {
                if let Some(n) = raw_motion_count {
                    c.goto_line(n.saturating_sub(1), &self.buffer, false);
                } else {
                    c.move_to_first_line(&self.buffer, false);
                }
                return self.linewise_range(start, c.position());
            }

            _ => return None,
        };

        let end = c.position();
        if start == end {
            return None;
        }

        // Order the range (motion might go backward).
        let (from, to) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        if inclusive {
            // Extend end to include the target character.
            let end_line_len = self.buffer.line_content_len(to.line).unwrap_or(0);
            let extended = if to.col < end_line_len {
                Position::new(to.line, to.col + 1)
            } else if to.line + 1 < self.buffer.line_count() {
                Position::new(to.line + 1, 0)
            } else {
                Position::new(to.line, end_line_len)
            };
            Some(Range::new(from, extended))
        } else {
            Some(Range::new(from, to))
        }
    }

    /// Compute a linewise range spanning from one position's line to another's.
    fn linewise_range(&self, a: Position, b: Position) -> Option<Range> {
        let first = a.line.min(b.line);
        let last = a.line.max(b.line);

        let start = Position::new(first, 0);
        let end = if last + 1 < self.buffer.line_count() {
            Position::new(last + 1, 0)
        } else {
            let len = self.buffer.line_len(last).unwrap_or(0);
            Position::new(last, len)
        };

        if start == end {
            return None;
        }

        Some(Range::new(start, end))
    }

    /// Resolve a text object key into a range.
    fn text_object_range(&self, code: KeyCode, inner: bool) -> Option<Range> {
        let pos = self.cursor.position();
        match code {
            KeyCode::Char('w') if inner => text_object::inner_word(&self.buffer, pos),
            KeyCode::Char('w') => text_object::a_word(&self.buffer, pos),
            KeyCode::Char('W') if inner => text_object::inner_big_word(&self.buffer, pos),
            KeyCode::Char('W') => text_object::a_big_word(&self.buffer, pos),
            KeyCode::Char('"') if inner => text_object::inner_double_quote(&self.buffer, pos),
            KeyCode::Char('"') => text_object::a_double_quote(&self.buffer, pos),
            KeyCode::Char('\'') if inner => text_object::inner_single_quote(&self.buffer, pos),
            KeyCode::Char('\'') => text_object::a_single_quote(&self.buffer, pos),
            KeyCode::Char('`') if inner => text_object::inner_backtick(&self.buffer, pos),
            KeyCode::Char('`') => text_object::a_backtick(&self.buffer, pos),
            KeyCode::Char('(' | ')' | 'b') if inner => {
                text_object::inner_paren(&self.buffer, pos)
            }
            KeyCode::Char('(' | ')' | 'b') => text_object::a_paren(&self.buffer, pos),
            KeyCode::Char('[' | ']') if inner => text_object::inner_square(&self.buffer, pos),
            KeyCode::Char('[' | ']') => text_object::a_square(&self.buffer, pos),
            KeyCode::Char('{' | '}' | 'B') if inner => {
                text_object::inner_curly(&self.buffer, pos)
            }
            KeyCode::Char('{' | '}' | 'B') => text_object::a_curly(&self.buffer, pos),
            KeyCode::Char('<' | '>') if inner => text_object::inner_angle(&self.buffer, pos),
            KeyCode::Char('<' | '>') => text_object::a_angle(&self.buffer, pos),
            _ => None,
        }
    }

    /// Apply an operator to a range.
    ///
    /// `linewise`: if true, the operation uses line-wise register semantics.
    fn apply_operator(&mut self, op: char, range: Range, linewise: bool) -> Action {
        if range.is_empty() {
            return Action::Continue;
        }

        let text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        let reg_kind = if linewise {
            RegisterKind::Line
        } else {
            RegisterKind::Char
        };
        let reg_text = if linewise && !text.ends_with('\n') {
            format!("{text}\n")
        } else {
            text.clone()
        };

        match op {
            'd' => {
                self.register.yank(reg_text, reg_kind);
                self.history.begin(self.cursor.position());
                self.history.record_delete(range.start, &text);
                self.buffer.delete(range);
                self.cursor
                    .set_position(range.start, &self.buffer, false);
                self.cursor.clamp(&self.buffer, false);
                if linewise {
                    self.cursor.move_to_first_non_blank(&self.buffer, false);
                }
                self.history.commit(self.cursor.position());
            }
            'c' => {
                self.register.yank(reg_text, reg_kind);
                self.history.begin(self.cursor.position());
                self.history.record_delete(range.start, &text);
                self.buffer.delete(range);
                self.cursor
                    .set_position(range.start, &self.buffer, true);
                self.cursor.clamp(&self.buffer, true);
                self.history.commit(self.cursor.position());
                // Begin a new transaction for the insert session.
                self.history.begin(self.cursor.position());
                self.mode = Mode::Insert;
            }
            'y' => {
                self.register.yank(reg_text, reg_kind);
                self.cursor
                    .set_position(range.start, &self.buffer, false);
                let lines = range.line_span();
                if lines > 1 {
                    self.set_message(format!("{lines} lines yanked"));
                }
            }
            _ => {}
        }

        Action::Continue
    }

    /// Apply a line-wise operator (dd, yy, cc) to `count` lines.
    ///
    /// `3dd` deletes 3 lines starting from the cursor's line. If there are
    /// fewer than `count` lines remaining, all lines from the cursor to the
    /// end of the buffer are affected.
    fn operator_line(&mut self, op: char, count: usize) -> Action {
        if self.buffer.is_empty() {
            return Action::Continue;
        }

        let line = self.cursor.line();
        let line_count = self.buffer.line_count();

        // The exclusive end line (first line NOT included).
        let end_line = (line + count).min(line_count);

        let range = if end_line < line_count {
            // Normal case: lines [line, end_line) with trailing newlines.
            Range::new(Position::new(line, 0), Position::new(end_line, 0))
        } else if line > 0 {
            // Deleting through end of buffer: eat the preceding newline.
            let prev_len = self.buffer.line_content_len(line - 1).unwrap_or(0);
            let last = line_count - 1;
            let last_len = self.buffer.line_len(last).unwrap_or(0);
            Range::new(
                Position::new(line - 1, prev_len),
                Position::new(last, last_len),
            )
        } else {
            // Deleting entire buffer.
            let last = line_count - 1;
            let last_len = self.buffer.line_len(last).unwrap_or(0);
            if last_len == 0 {
                return Action::Continue;
            }
            Range::new(Position::ZERO, Position::new(last, last_len))
        };

        self.apply_operator(op, range, true)
    }

    // ── Insert mode ─────────────────────────────────────────────────────

    fn handle_insert(&mut self, key: &KeyEvent) -> Action {
        // Clear message on first keypress in insert mode.
        self.clear_message();

        // Record all insert-mode keys for dot-repeat (including Esc).
        if self.dot_recording && !self.dot_replaying {
            self.dot_keys.push(*key);
        }

        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        match key.code {
            KeyCode::Escape => {
                // Commit the insert-mode transaction and return to normal.
                self.history.commit(self.cursor.position());
                self.mode = Mode::Normal;
                self.cursor.move_left(1, &self.buffer, false);

                // Finalize dot-repeat recording (covers i/a/o/O/I/A + text
                // and c + motion + text).
                if self.dot_recording && !self.dot_replaying {
                    self.dot_finish();
                }
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

    // ── Visual mode ────────────────────────────────────────────────────

    #[allow(clippy::too_many_lines)]
    fn handle_visual(&mut self, key: &KeyEvent) -> Action {
        self.clear_message();

        let pe = self.mode.cursor_past_end();

        // Extract the current visual kind.
        let Mode::Visual(current_kind) = self.mode else {
            return Action::Continue;
        };

        // Ctrl combinations cancel any accumulated count.
        if key.modifiers.contains(Modifiers::CTRL) {
            self.count = None;
            match key.code {
                KeyCode::Char('c') => return Action::Quit,
                KeyCode::Char('v') => {
                    // Toggle: Ctrl-V in Block → Normal, otherwise → Block.
                    if current_kind == VisualKind::Block {
                        self.cursor.clear_anchor();
                        self.mode = Mode::Normal;
                    } else {
                        self.mode = Mode::Visual(VisualKind::Block);
                    }
                    return Action::Continue;
                }
                _ => {}
            }
        }

        // Handle pending char find (f/F/t/T waiting for target char).
        if let Some(pending) = self.pending.take() {
            if let Pending::CharFind { kind, count } = pending {
                if let KeyCode::Char(ch) = key.code {
                    self.last_char_find = Some((ch, kind));
                    self.execute_char_find_motion(ch, kind, count, pe);
                }
            }
            return Action::Continue;
        }

        // Digit accumulation (same logic as normal mode).
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.push_count_digit(d as u8 - b'0');
                return Action::Continue;
            }
            KeyCode::Char('0') if self.count.is_some() => {
                self.push_count_digit(0);
                return Action::Continue;
            }
            _ => {}
        }

        let raw_count = self.take_raw_count();
        let count = raw_count.unwrap_or(1);

        // Try motion keys (shared with normal mode). Motions move the
        // cursor but leave the anchor in place, extending the selection.
        if self.apply_motion(key.code, pe, raw_count) {
            return Action::Continue;
        }

        match key.code {
            KeyCode::Escape => {
                self.cursor.clear_anchor();
                self.mode = Mode::Normal;
            }

            // -- Character find (enter pending, waiting for target char) --
            KeyCode::Char('f') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::Forward,
                    count,
                });
            }
            KeyCode::Char('F') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::Backward,
                    count,
                });
            }
            KeyCode::Char('t') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::TillForward,
                    count,
                });
            }
            KeyCode::Char('T') => {
                self.pending = Some(Pending::CharFind {
                    kind: CharFindKind::TillBackward,
                    count,
                });
            }

            // -- Mode toggles --
            KeyCode::Char('v') => {
                if current_kind == VisualKind::Char {
                    self.cursor.clear_anchor();
                    self.mode = Mode::Normal;
                } else {
                    self.mode = Mode::Visual(VisualKind::Char);
                }
            }
            KeyCode::Char('V') => {
                if current_kind == VisualKind::Line {
                    self.cursor.clear_anchor();
                    self.mode = Mode::Normal;
                } else {
                    self.mode = Mode::Visual(VisualKind::Line);
                }
            }

            // -- Swap anchor and cursor --
            KeyCode::Char('o') => {
                if let Some(anchor) = self.cursor.anchor() {
                    let pos = self.cursor.position();
                    self.cursor.set_position(anchor, &self.buffer, pe);
                    self.cursor.set_anchor_at(pos);
                }
            }

            // -- Operators --
            KeyCode::Char('d' | 'x') => self.visual_delete(),
            KeyCode::Char('y') => self.visual_yank(),
            KeyCode::Char('c') => self.visual_change(),

            _ => {}
        }

        Action::Continue
    }

    // ── Visual selection ranges ──────────────────────────────────────────

    /// Compute the effective char-wise selection range.
    ///
    /// Extends the half-open range from `cursor.selection()` to include the
    /// cursor character (Vim visual mode is inclusive at both ends).
    fn visual_char_range(&self) -> Option<Range> {
        let range = self.cursor.selection()?;
        let end_line_len = self.buffer.line_content_len(range.end.line).unwrap_or(0);

        let end = if range.end.col < end_line_len {
            // Normal case: extend by 1 char.
            Position::new(range.end.line, range.end.col + 1)
        } else if range.end.line + 1 < self.buffer.line_count() {
            // At end of line — wrap to next line to include the newline.
            Position::new(range.end.line + 1, 0)
        } else {
            // Last char of last line — clamp to content length.
            Position::new(range.end.line, end_line_len)
        };

        Some(Range::new(range.start, end))
    }

    /// Compute the effective line-wise selection range.
    ///
    /// Expands to full lines including trailing newlines.
    fn visual_line_range(&self) -> Option<Range> {
        let range = self.cursor.selection()?;
        let start_line = range.start.line;
        let end_line = range.end.line;

        if end_line + 1 < self.buffer.line_count() {
            // Not the last line — include through the trailing newline.
            Some(Range::new(
                Position::new(start_line, 0),
                Position::new(end_line + 1, 0),
            ))
        } else if start_line > 0 {
            // Selection includes the last line — eat the preceding newline
            // so we don't leave a trailing blank line after deletion.
            let prev_len = self.buffer.line_content_len(start_line - 1).unwrap_or(0);
            let last_len = self.buffer.line_len(end_line).unwrap_or(0);
            Some(Range::new(
                Position::new(start_line - 1, prev_len),
                Position::new(end_line, last_len),
            ))
        } else {
            // Entire buffer selected line-wise.
            let last_len = self.buffer.line_len(end_line).unwrap_or(0);
            Some(Range::new(Position::ZERO, Position::new(end_line, last_len)))
        }
    }

    // ── Visual operators ─────────────────────────────────────────────────

    /// Delete the visual selection (`d` / `x` in visual mode).
    fn visual_delete(&mut self) {
        let Mode::Visual(kind) = self.mode else { return };

        if kind == VisualKind::Block {
            self.set_error("Block operations not yet supported");
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        }

        let reg_kind = match kind {
            VisualKind::Char => RegisterKind::Char,
            VisualKind::Line | VisualKind::Block => RegisterKind::Line,
        };

        let range = match kind {
            VisualKind::Char => self.visual_char_range(),
            VisualKind::Line | VisualKind::Block => self.visual_line_range(),
        };

        let Some(range) = range else {
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        };

        // Extract text before deletion (for the register).
        let text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Ensure line-wise register text ends with newline.
        let text = if reg_kind == RegisterKind::Line && !text.ends_with('\n') {
            text + "\n"
        } else {
            text
        };

        self.register.yank(text.clone(), reg_kind);

        self.history.begin(self.cursor.position());
        self.history.record_delete(range.start, &text);
        self.buffer.delete(range);
        self.cursor.clear_anchor();
        self.cursor
            .set_position(range.start, &self.buffer, false);
        self.cursor.clamp(&self.buffer, false);
        self.history.commit(self.cursor.position());
        self.mode = Mode::Normal;
    }

    /// Yank the visual selection (`y` in visual mode).
    fn visual_yank(&mut self) {
        let Mode::Visual(kind) = self.mode else { return };

        if kind == VisualKind::Block {
            self.set_error("Block operations not yet supported");
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        }

        let (range, reg_kind) = match kind {
            VisualKind::Char => {
                let Some(r) = self.visual_char_range() else {
                    self.cursor.clear_anchor();
                    self.mode = Mode::Normal;
                    return;
                };
                (r, RegisterKind::Char)
            }
            VisualKind::Line | VisualKind::Block => {
                // For yank, we want the clean line range (full lines).
                let Some(r) = self.cursor.selection() else {
                    self.cursor.clear_anchor();
                    self.mode = Mode::Normal;
                    return;
                };
                let start = Position::new(r.start.line, 0);
                let end_line = r.end.line;
                let end = if end_line + 1 < self.buffer.line_count() {
                    Position::new(end_line + 1, 0)
                } else {
                    let len = self.buffer.line_len(end_line).unwrap_or(0);
                    Position::new(end_line, len)
                };
                (Range::new(start, end), RegisterKind::Line)
            }
        };

        let text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Ensure line-wise register text ends with newline.
        let text = if reg_kind == RegisterKind::Line && !text.ends_with('\n') {
            text + "\n"
        } else {
            text
        };

        let line_count = range.line_span();
        self.register.yank(text, reg_kind);

        // Move cursor to start of selection (Vim behavior).
        let start = range.start;
        self.cursor.clear_anchor();
        self.cursor.set_position(start, &self.buffer, false);
        self.mode = Mode::Normal;

        if line_count > 1 {
            self.set_message(format!("{line_count} lines yanked"));
        }
    }

    /// Change the visual selection (`c` in visual mode).
    ///
    /// Deletes the selection and enters insert mode.
    fn visual_change(&mut self) {
        let Mode::Visual(kind) = self.mode else { return };

        if kind == VisualKind::Block {
            self.set_error("Block operations not yet supported");
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        }

        let reg_kind = match kind {
            VisualKind::Char => RegisterKind::Char,
            VisualKind::Line | VisualKind::Block => RegisterKind::Line,
        };

        let range = match kind {
            VisualKind::Char => self.visual_char_range(),
            VisualKind::Line | VisualKind::Block => self.visual_line_range(),
        };

        let Some(range) = range else {
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        };

        let text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        let text = if reg_kind == RegisterKind::Line && !text.ends_with('\n') {
            text + "\n"
        } else {
            text
        };

        self.register.yank(text.clone(), reg_kind);

        // Delete the selection as one transaction, then begin a new one
        // for the insert phase (so undo restores text, redo re-deletes).
        self.history.begin(self.cursor.position());
        self.history.record_delete(range.start, &text);
        self.buffer.delete(range);
        self.cursor.clear_anchor();
        self.cursor
            .set_position(range.start, &self.buffer, true);
        self.cursor.clamp(&self.buffer, true);
        self.history.commit(self.cursor.position());

        // Begin a new transaction for the insert session.
        self.history.begin(self.cursor.position());
        self.mode = Mode::Insert;
    }

    // ── Search mode ─────────────────────────────────────────────────────

    /// Handle input while the search prompt is active.
    fn handle_search(&mut self, key: &KeyEvent) -> Action {
        if key.modifiers.contains(Modifiers::CTRL) && key.code == KeyCode::Char('c') {
            // Cancel search (same as Escape).
            self.cancel_search();
            return Action::Continue;
        }

        match key.code {
            KeyCode::Escape => {
                self.cancel_search();
            }

            KeyCode::Enter => {
                self.confirm_search();
            }

            KeyCode::Char(ch) => {
                if let Some(ref mut ss) = self.search {
                    ss.insert_char(ch);
                }
                self.incremental_search();
            }

            KeyCode::Backspace => {
                let should_cancel = self.search.as_mut().is_some_and(|ss| {
                    if ss.backspace() {
                        false
                    } else {
                        // Backspace on empty input: cancel like Vim.
                        ss.is_empty()
                    }
                });

                if should_cancel {
                    self.cancel_search();
                } else {
                    self.incremental_search();
                }
            }

            _ => {}
        }

        Action::Continue
    }

    /// Start a search session in the given direction.
    fn start_search(&mut self, direction: SearchDirection) {
        self.clear_message();
        let saved_pos = self.cursor.position();
        let saved_top = self.view.top_line();
        self.search = Some(SearchState::new(direction, saved_pos, saved_top));
    }

    /// Cancel the active search and restore the cursor.
    fn cancel_search(&mut self) {
        if let Some(ss) = self.search.take() {
            self.cursor
                .set_position(ss.saved_pos(), &self.buffer, false);
            self.view.set_top_line(ss.saved_top_line());
        }
    }

    /// Confirm the search: store the pattern for n/N and exit search mode.
    fn confirm_search(&mut self) {
        if let Some(ss) = self.search.take() {
            let pattern = ss.input().to_string();
            let direction = ss.direction();
            if pattern.is_empty() {
                // Empty Enter: restore cursor (no search performed).
                self.cursor
                    .set_position(ss.saved_pos(), &self.buffer, false);
                self.view.set_top_line(ss.saved_top_line());
            } else {
                self.last_search = pattern;
                self.last_search_direction = direction;
            }
        }
    }

    /// Perform incremental search: jump to the next match as the user types.
    fn incremental_search(&mut self) {
        let (pattern, direction, saved_pos) = match &self.search {
            Some(ss) => (ss.input().to_string(), ss.direction(), ss.saved_pos()),
            None => return,
        };

        if pattern.is_empty() {
            // Empty pattern: restore to saved position.
            if let Some(ref ss) = self.search {
                self.cursor
                    .set_position(ss.saved_pos(), &self.buffer, false);
                self.view.set_top_line(ss.saved_top_line());
            }
            return;
        }

        // Search from the saved position (where the cursor was before `/`).
        if let Some(m) = search::find(&self.buffer, &pattern, saved_pos, direction) {
            self.cursor
                .set_position(m.start, &self.buffer, false);
        }
    }

    /// Jump to the next match of the last search pattern (`n` in normal mode).
    fn search_next(&mut self) {
        if self.last_search.is_empty() {
            self.set_error("E486: Pattern not found");
            return;
        }

        let from = Position::new(self.cursor.line(), self.cursor.col() + 1);
        if let Some(m) = search::find(
            &self.buffer,
            &self.last_search,
            from,
            self.last_search_direction,
        ) {
            let wrapped = match self.last_search_direction {
                SearchDirection::Forward => m.start < self.cursor.position(),
                SearchDirection::Backward => m.start > self.cursor.position(),
            };
            self.cursor
                .set_position(m.start, &self.buffer, false);
            if wrapped {
                let msg = match self.last_search_direction {
                    SearchDirection::Forward => {
                        "search hit BOTTOM, continuing at TOP"
                    }
                    SearchDirection::Backward => {
                        "search hit TOP, continuing at BOTTOM"
                    }
                };
                self.set_message(msg);
            }
        } else {
            self.set_error(format!(
                "E486: Pattern not found: {}",
                self.last_search
            ));
        }
    }

    /// Jump to the previous match (`N` in normal mode — opposite direction).
    fn search_prev(&mut self) {
        if self.last_search.is_empty() {
            self.set_error("E486: Pattern not found");
            return;
        }

        let opposite = self.last_search_direction.opposite();

        // For backward from current position: search from col - 1 (or wrap).
        let from = if self.cursor.col() > 0 {
            Position::new(self.cursor.line(), self.cursor.col() - 1)
        } else if self.cursor.line() > 0 {
            let prev_line = self.cursor.line() - 1;
            let prev_len = self.buffer.line_content_len(prev_line).unwrap_or(0);
            Position::new(prev_line, prev_len.saturating_sub(1))
        } else {
            // At (0,0): wrap to end of buffer.
            let last_line = self.buffer.line_count().saturating_sub(1);
            let last_len = self.buffer.line_content_len(last_line).unwrap_or(0);
            Position::new(last_line, last_len.saturating_sub(1))
        };

        if let Some(m) = search::find(&self.buffer, &self.last_search, from, opposite) {
            let wrapped = match opposite {
                SearchDirection::Forward => m.start < self.cursor.position(),
                SearchDirection::Backward => m.start > self.cursor.position(),
            };
            self.cursor
                .set_position(m.start, &self.buffer, false);
            if wrapped {
                let msg = match opposite {
                    SearchDirection::Forward => {
                        "search hit BOTTOM, continuing at TOP"
                    }
                    SearchDirection::Backward => {
                        "search hit TOP, continuing at BOTTOM"
                    }
                };
                self.set_message(msg);
            }
        } else {
            self.set_error(format!(
                "E486: Pattern not found: {}",
                self.last_search
            ));
        }
    }

    /// Search for the word under the cursor (`*` forward, `#` backward).
    fn search_word_under_cursor(&mut self, direction: SearchDirection) {
        if let Some(word) = search::word_under_cursor(&self.buffer, self.cursor.position()) {
            self.last_search = word;
            self.last_search_direction = direction;
            self.search_next();
        } else {
            self.set_error("E348: No string under cursor");
        }
    }

    // ── Paste commands ──────────────────────────────────────────────────

    /// Paste after the cursor (`p` / `3p` in normal mode).
    ///
    /// With count, the register content is pasted `count` times.
    fn paste_after(&mut self, count: usize) {
        if self.register.is_empty() || count == 0 {
            return;
        }

        let single = self.register.content().to_string();
        let text = single.repeat(count);
        let kind = self.register.kind();

        let pos = match kind {
            RegisterKind::Char => {
                // Insert after the cursor character.
                let line_len = self.buffer.line_content_len(self.cursor.line()).unwrap_or(0);
                if line_len == 0 {
                    self.cursor.position()
                } else {
                    Position::new(self.cursor.line(), self.cursor.col() + 1)
                }
            }
            RegisterKind::Line => {
                // Insert on the line below.
                if self.cursor.line() + 1 < self.buffer.line_count() {
                    Position::new(self.cursor.line() + 1, 0)
                } else {
                    // Last line: insert after content, prepend a newline.
                    let len = self.buffer.line_len(self.cursor.line()).unwrap_or(0);
                    Position::new(self.cursor.line(), len)
                }
            }
        };

        self.history.begin(self.cursor.position());

        if kind == RegisterKind::Line && self.cursor.line() + 1 >= self.buffer.line_count() {
            // At last line: insert newline first, then the text.
            let insert_text = format!("\n{text}");
            // Strip trailing newline from text so we don't get an extra blank.
            let insert_text = insert_text.trim_end_matches('\n').to_string() + "\n";
            let trimmed = insert_text.trim_end_matches('\n');
            self.history.record_insert(pos, trimmed);
            self.buffer.insert(pos, trimmed);
            self.cursor
                .set_position(Position::new(self.cursor.line() + 1, 0), &self.buffer, false);
        } else if kind == RegisterKind::Line {
            self.history.record_insert(pos, &text);
            self.buffer.insert(pos, &text);
            self.cursor.set_position(pos, &self.buffer, false);
        } else {
            self.history.record_insert(pos, &text);
            self.buffer.insert(pos, &text);
            // Place cursor at end of pasted text (Vim puts cursor on last
            // pasted char, not after it).
            if text.len() > 1 {
                let end = Position::new(pos.line, pos.col + text.chars().count() - 1);
                self.cursor.set_position(end, &self.buffer, false);
            } else {
                self.cursor.set_position(pos, &self.buffer, false);
            }
        }

        self.cursor.clamp(&self.buffer, false);
        self.history.commit(self.cursor.position());
    }

    /// Paste before the cursor (`P` / `3P` in normal mode).
    ///
    /// With count, the register content is pasted `count` times.
    fn paste_before(&mut self, count: usize) {
        if self.register.is_empty() || count == 0 {
            return;
        }

        let single = self.register.content().to_string();
        let text = single.repeat(count);
        let kind = self.register.kind();

        let pos = match kind {
            RegisterKind::Char => self.cursor.position(),
            RegisterKind::Line => Position::new(self.cursor.line(), 0),
        };

        self.history.begin(self.cursor.position());
        self.history.record_insert(pos, &text);
        self.buffer.insert(pos, &text);

        if kind == RegisterKind::Line {
            self.cursor.set_position(pos, &self.buffer, false);
        } else if text.chars().count() > 1 {
            let end = Position::new(pos.line, pos.col + text.chars().count() - 1);
            self.cursor.set_position(end, &self.buffer, false);
        }

        self.cursor.clamp(&self.buffer, false);
        self.history.commit(self.cursor.position());
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

    /// Delete `count` characters at the cursor (`x` / `3x` in Vim).
    ///
    /// Stores the deleted text in the unnamed register (Vim behavior:
    /// every delete is also a cut). Does not cross line boundaries.
    fn delete_chars_at_cursor(&mut self, count: usize) {
        let pe = self.mode.cursor_past_end();
        let pos = self.cursor.position();
        let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);

        if line_len == 0 || pos.col >= line_len {
            return;
        }

        let end_col = (pos.col + count).min(line_len);
        let to = Position::new(pos.line, end_col);
        let range = Range::new(pos, to);

        let text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        self.register.yank(text.clone(), RegisterKind::Char);
        self.history.begin(pos);
        self.history.record_delete(pos, &text);
        self.buffer.delete(range);
        self.cursor.clamp(&self.buffer, pe);
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

        // Search-input mode takes priority: if the user is typing a search
        // pattern, all keys go to the search handler.
        if self.search.is_some() {
            return self.handle_search(key);
        }

        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            Mode::Visual(_) => self.handle_visual(key),
            // Replace mode — not yet implemented.
            Mode::Replace => Action::Continue,
        }
    }

    fn on_resize(&mut self, _size: Size) {
        // The event loop already resized the framebuffer. The view will
        // adjust scroll on the next paint via ensure_cursor_visible.
    }

    fn paint(&mut self, frame: &mut FrameBuffer) {
        let w = frame.width();
        let h = frame.height();

        // Compute the visual selection for the render pipeline.
        let selection = match self.mode {
            Mode::Visual(kind) => self.cursor.selection().map(|r| (r, kind)),
            _ => None,
        };

        if h < 2 {
            // Too small for text + status + command line. Just render
            // what we can into the View.
            self.cursor_screen = self.view.render(
                &self.buffer,
                &self.cursor,
                self.mode,
                selection,
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
            selection,
            frame,
            0,
            0,
            w,
            view_height,
        );

        // Highlight search matches in the visible area.
        let hl_pattern = if self.search.is_some() {
            self.search.as_ref().map_or("", |ss| ss.input())
        } else {
            &self.last_search
        };
        if !hl_pattern.is_empty() {
            view::highlight_matches(
                &self.view,
                frame,
                &self.buffer,
                hl_pattern,
                0,
                0,
                w,
                view_height,
            );
        }

        // Bottom row: command line, search prompt, or message.
        let bottom_y = h - 1;

        if let Some(ref ss) = self.search {
            // Render the search prompt and position the cursor there.
            let search_cursor = view::render_search_line(
                frame,
                ss.prefix(),
                ss.input(),
                ss.input_cursor(),
                0,
                bottom_y,
                w,
            );
            self.cursor_screen = search_cursor;
        } else if self.mode == Mode::Command {
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use n_term::input::KeyEventKind;

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Create a key press event for a character.
    fn press(ch: char) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })
    }

    /// Create an Escape key press event.
    fn esc() -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Escape,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })
    }

    /// Create an Enter key press event.
    fn enter() -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })
    }

    /// Create a Backspace key press event.
    fn backspace() -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Backspace,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })
    }

    /// Feed a sequence of events to the editor.
    fn feed(editor: &mut Editor, events: &[Event]) {
        for event in events {
            editor.on_event(event);
        }
    }

    /// Create an editor with the given text in the buffer.
    fn editor_with(text: &str) -> Editor {
        let mut e = Editor::new();
        e.buffer = Buffer::from_text(text);
        e
    }

    // ── merge_counts ──────────────────────────────────────────────────────

    #[test]
    fn merge_counts_both_none() {
        assert_eq!(Editor::merge_counts(None, None), None);
    }

    #[test]
    fn merge_counts_first_some() {
        assert_eq!(Editor::merge_counts(Some(3), None), Some(3));
    }

    #[test]
    fn merge_counts_second_some() {
        assert_eq!(Editor::merge_counts(None, Some(5)), Some(5));
    }

    #[test]
    fn merge_counts_both_some() {
        assert_eq!(Editor::merge_counts(Some(2), Some(3)), Some(6));
    }

    // ── Dot-repeat: x (delete char) ──────────────────────────────────────

    #[test]
    fn dot_repeat_x() {
        let mut e = editor_with("abcdef");
        // x deletes 'a', cursor on 'b', then . deletes 'b'
        feed(&mut e, &[press('x'), press('.')]);
        assert_eq!(e.buffer.contents(), "cdef");
    }

    #[test]
    fn dot_repeat_x_with_count() {
        let mut e = editor_with("abcdef");
        // 2x deletes 'ab', . repeats 2x → deletes 'cd'
        feed(&mut e, &[press('2'), press('x'), press('.')]);
        assert_eq!(e.buffer.contents(), "ef");
    }

    #[test]
    fn dot_repeat_x_count_override() {
        let mut e = editor_with("abcdefgh");
        // 2x deletes 'ab', 3. repeats with count 3 → deletes 'cde'
        feed(
            &mut e,
            &[press('2'), press('x'), press('3'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "fgh");
    }

    // ── Dot-repeat: dd (delete line) ─────────────────────────────────────

    #[test]
    fn dot_repeat_dd() {
        let mut e = editor_with("first\nsecond\nthird\nfourth");
        // dd deletes "first", . deletes "second"
        feed(&mut e, &[press('d'), press('d'), press('.')]);
        assert_eq!(e.buffer.contents(), "third\nfourth");
    }

    #[test]
    fn dot_repeat_dd_with_count() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf");
        // 2dd deletes "a" and "b", . repeats 2dd → deletes "c" and "d"
        feed(
            &mut e,
            &[press('2'), press('d'), press('d'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "e\nf");
    }

    // ── Dot-repeat: dw (delete word) ─────────────────────────────────────

    #[test]
    fn dot_repeat_dw() {
        let mut e = editor_with("one two three four");
        // dw deletes "one ", . deletes "two "
        feed(&mut e, &[press('d'), press('w'), press('.')]);
        assert_eq!(e.buffer.contents(), "three four");
    }

    #[test]
    fn dot_repeat_dw_count_override() {
        let mut e = editor_with("one two three four five six");
        // dw deletes "one ", 2. deletes 2 words ("two three ")
        feed(
            &mut e,
            &[press('d'), press('w'), press('2'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "four five six");
    }

    // ── Dot-repeat: diw (delete inner word — text object) ────────────────

    #[test]
    fn dot_repeat_diw() {
        let mut e = editor_with("hello world");
        // diw deletes "hello", w to next word, . deletes "world"
        feed(
            &mut e,
            &[press('d'), press('i'), press('w'), press('w'), press('.')],
        );
        // After diw on "hello": " world" remains, cursor on space.
        // w moves to "world". . does diw on "world".
        assert_eq!(e.buffer.contents(), " ");
    }

    // ── Dot-repeat: p (paste) ────────────────────────────────────────────

    #[test]
    fn dot_repeat_paste() {
        let mut e = editor_with("abcde");
        // x to cut 'a', p to paste after cursor, . to paste again
        feed(&mut e, &[press('x'), press('p'), press('.')]);
        // x cuts 'a' → "bcde", p pastes 'a' after 'b' → "baacde"... wait
        // Actually: x on 'a' → "bcde" cursor on 'b', p pastes 'a' after 'b' → "bacde"
        // cursor on 'a'. . pastes again → "baacde"... hmm let me think.
        // x: buffer="bcde" cursor=(0,0) on 'b'. register='a'
        // p: paste after cursor char. pos = (0,1). buffer="bacde" cursor=(0,1) on 'a'.
        // .: replays p. paste 'a' after cursor (0,1). pos=(0,2). buffer="baacde" cursor=(0,2)
        assert_eq!(e.buffer.contents(), "baacde");
    }

    // ── Dot-repeat: insert mode (i + text + Esc) ────────────────────────

    #[test]
    fn dot_repeat_insert() {
        let mut e = editor_with("ab");
        // ihello<Esc> inserts "hello" before 'a', . inserts "hello" again
        feed(
            &mut e,
            &[
                press('i'),
                press('h'),
                press('e'),
                press('l'),
                press('l'),
                press('o'),
                esc(),
                press('.'),
            ],
        );
        // After ihello<Esc>: "helloab", cursor on 'o' (col 4).
        // Esc moves left to 'o' (col 4). . replays: i at col 4, types "hello",
        // Esc. Result: "hellhelloab"... wait.
        // After first ihello<Esc>: buffer="helloab", cursor at col 4 ('o').
        // . replays [i, h, e, l, l, o, Esc].
        // i: enters insert at col 4. Types "hello" → "hellhellooab". Esc.
        // Wait: insert at col 4 means inserting before 'o'. So "hell" + "hello" + "oab" = "hellhellooab"
        assert_eq!(e.buffer.contents(), "hellhellooab");
    }

    #[test]
    fn dot_repeat_append() {
        let mut e = editor_with("ab");
        // aX<Esc> appends 'X' after 'a', move to 'b', . appends 'X' after 'b'
        feed(
            &mut e,
            &[press('a'), press('X'), esc(), press('l'), press('.')],
        );
        // a: cursor moves right (past 'a' to 'b' pos), enters insert.
        // X: inserts 'X' at col 1. buffer="aXb". cursor at col 2 ('b').
        // Esc: commit, move left to col 1 ('X').
        // l: move right to col 2 ('b').
        // .: replays [a, X, Esc]. a moves right to col 3 (past 'b'), enters insert.
        // X: inserts at col 3. buffer="aXbX". Esc: move left to col 3 ('X').
        assert_eq!(e.buffer.contents(), "aXbX");
    }

    // ── Dot-repeat: o (open line below) ─────────────────────────────────

    #[test]
    fn dot_repeat_open_line_below() {
        let mut e = editor_with("first\nthird");
        // ohello<Esc> opens line below and types "hello"
        // j moves down to "third"
        // . opens line below "third" and types "hello"
        feed(
            &mut e,
            &[
                press('o'),
                press('h'),
                press('e'),
                press('l'),
                press('l'),
                press('o'),
                esc(),
                press('j'),
                press('.'),
            ],
        );
        assert_eq!(e.buffer.contents(), "first\nhello\nthird\nhello");
    }

    // ── Dot-repeat: ciw (change inner word) ─────────────────────────────

    #[test]
    fn dot_repeat_ciw() {
        let mut e = editor_with("foo bar baz");
        // ciw changes "foo" to "X"
        feed(
            &mut e,
            &[
                press('c'),
                press('i'),
                press('w'),
                press('X'),
                esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "X bar baz");
        // Move to "bar": w w (past space, to 'bar')
        feed(&mut e, &[press('w')]);
        // . changes "bar" to "X"
        feed(&mut e, &[press('.')]);
        assert_eq!(e.buffer.contents(), "X X baz");
    }

    // ── Dot-repeat: cc (change line) ────────────────────────────────────

    #[test]
    fn dot_repeat_cc() {
        let mut e = editor_with("first\nsecond\nthird");
        // cc deletes the line (including newline), enters insert.
        // Note: our cc uses the same linewise range as dd (deletes newline).
        feed(
            &mut e,
            &[
                press('c'),
                press('c'),
                press('h'),
                press('e'),
                press('l'),
                press('l'),
                press('o'),
                esc(),
            ],
        );
        // "first\n" deleted → "second\nthird", then "hello" typed at start.
        assert_eq!(e.buffer.contents(), "hellosecond\nthird");
        // . replays cc + "hello" + Esc on current line
        feed(&mut e, &[press('.')]);
        assert_eq!(e.buffer.contents(), "hellothird");
    }

    // ── Dot-repeat: operator + motion count (2d3w) ──────────────────────

    #[test]
    fn dot_repeat_2d3w_effective_count() {
        // 24 single-letter words. 2d3w deletes 6 words each time.
        let mut e = editor_with("a b c d e f g h i j k l m n o p q r s t u v w x");
        // 2d3w: effective count = 6 words
        feed(
            &mut e,
            &[
                press('2'),
                press('d'),
                press('3'),
                press('w'),
            ],
        );
        assert_eq!(e.buffer.contents(), "g h i j k l m n o p q r s t u v w x");
        // . repeats with same effective count (6 words)
        feed(&mut e, &[press('.')]);
        assert_eq!(e.buffer.contents(), "m n o p q r s t u v w x");
    }

    #[test]
    fn dot_repeat_2d3w_count_override() {
        let mut e = editor_with("a b c d e f g h i j k l");
        // 2d3w: deletes 6 words ("a b c d e f ")
        feed(
            &mut e,
            &[
                press('2'),
                press('d'),
                press('3'),
                press('w'),
            ],
        );
        assert_eq!(e.buffer.contents(), "g h i j k l");
        // 2. overrides with count 2 → deletes 2 words ("g h ")
        feed(&mut e, &[press('2'), press('.')]);
        assert_eq!(e.buffer.contents(), "i j k l");
    }

    // ── Dot-repeat: no prior change ─────────────────────────────────────

    #[test]
    fn dot_repeat_no_prior_change() {
        let mut e = editor_with("hello");
        // . with no prior change is a no-op
        feed(&mut e, &[press('.')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    // ── Dot-repeat: d<Esc> cancels, preserves previous change ────────────

    #[test]
    fn dot_cancel_preserves_previous() {
        let mut e = editor_with("abcdef");
        // x deletes 'a', d<Esc> cancels, . still repeats x
        feed(&mut e, &[press('x'), press('d'), esc(), press('.')]);
        assert_eq!(e.buffer.contents(), "cdef");
    }

    // ── Dot-repeat: yank does NOT overwrite last change ──────────────────

    #[test]
    fn yank_does_not_overwrite_last_change() {
        let mut e = editor_with("abcdef");
        // x deletes 'a', yw yanks (not a change), . repeats x
        feed(
            &mut e,
            &[press('x'), press('y'), press('w'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "cdef");
    }

    // ── Dot-repeat: insert with backspace ───────────────────────────────

    #[test]
    fn dot_repeat_insert_with_backspace() {
        let mut e = editor_with("ab");
        // ixy<BS>z<Esc> types "xz" (types x, y, deletes y, types z)
        feed(
            &mut e,
            &[
                press('i'),
                press('x'),
                press('y'),
                backspace(),
                press('z'),
                esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "xzab");

        // Move right past 'z' to 'a', . replays the same edit
        feed(&mut e, &[press('l'), press('.')]);
        // Insert at cursor col 2 ('a'): types x, y, backspace, z → "xz"
        // Buffer becomes "xzxzab"... let me trace:
        // After first edit: "xzab", cursor at col 1 ('z') after Esc move_left.
        // l: cursor at col 2 ('a').
        // .: replays [i, x, y, BS, z, Esc].
        // i at col 2: insert mode. x → "xzxab". y → "xzxyab". BS → "xzxab". z → "xzxzab". Esc.
        assert_eq!(e.buffer.contents(), "xzxzab");
    }

    // ── Dot-repeat: open line above (O) ──────────────────────────────────

    #[test]
    fn dot_repeat_open_line_above() {
        let mut e = editor_with("second\nfourth");
        // Ohi<Esc> opens line above "second" and types "hi"
        feed(
            &mut e,
            &[press('O'), press('h'), press('i'), esc()],
        );
        assert_eq!(e.buffer.contents(), "hi\nsecond\nfourth");

        // Move to "fourth" (down, down), . opens line above "fourth"
        feed(&mut e, &[press('j'), press('j'), press('.')]);
        assert_eq!(e.buffer.contents(), "hi\nsecond\nhi\nfourth");
    }

    // ── Dot-repeat: d$ (delete to end of line) ─────────────────────────

    #[test]
    fn dot_repeat_d_dollar() {
        let mut e = editor_with("hello world\nfoo barbaz");
        // Move to col 6 ('w'), d$ deletes "world"
        feed(
            &mut e,
            &[
                press('l'),
                press('l'),
                press('l'),
                press('l'),
                press('l'),
                press('l'), // cursor at col 6, 'w'
                press('d'),
                press('$'),
            ],
        );
        assert_eq!(e.buffer.contents(), "hello \nfoo barbaz");

        // After d$, cursor is set to range.start (0,6), clamped to (0,5).
        // set_position clamps THEN sets sticky_col, so sticky_col = 5.
        // j moves to line 1 col 5 ('a' in "barbaz"). d$ deletes "arbaz".
        feed(&mut e, &[press('j'), press('.')]);
        assert_eq!(e.buffer.contents(), "hello \nfoo b");
    }

    // ── Dot-repeat: I (insert at first non-blank) ───────────────────────

    #[test]
    fn dot_repeat_insert_at_first_non_blank() {
        let mut e = editor_with("  hello\n  world");
        // I inserts at first non-blank (col 2), types ">>", Esc
        feed(
            &mut e,
            &[press('I'), press('>'), press('>'), esc()],
        );
        assert_eq!(e.buffer.contents(), "  >>hello\n  world");

        // j to next line, . repeats
        feed(&mut e, &[press('j'), press('.')]);
        assert_eq!(e.buffer.contents(), "  >>hello\n  >>world");
    }

    // ── Dot-repeat: insert with Enter ────────────────────────────────────

    #[test]
    fn dot_repeat_insert_with_enter() {
        let mut e = editor_with("ab");
        // iX<Enter>Y<Esc>: inserts "X\nY" before 'a'
        feed(
            &mut e,
            &[press('i'), press('X'), enter(), press('Y'), esc()],
        );
        assert_eq!(e.buffer.contents(), "X\nYab");

        // Move to end of first line, . repeats
        feed(&mut e, &[press('g'), press('.')]);
        // g goes to first line. Cursor at 'X' (col 0).
        // . replays [i, X, Enter, Y, Esc]
        // i at col 0, types X → "XX\nYab". Enter → "X\nX\nYab". Y → "X\nYX\nYab". Esc.
        assert_eq!(e.buffer.contents(), "X\nYX\nYab");
    }

    // ── Dot-repeat: cw (change word) ────────────────────────────────────

    #[test]
    fn dot_repeat_cw() {
        let mut e = editor_with("old old old");
        // cw changes "old" to "new"
        feed(
            &mut e,
            &[
                press('c'),
                press('w'),
                press('n'),
                press('e'),
                press('w'),
                esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "new old old");

        // Move to next "old", . changes it to "new"
        feed(&mut e, &[press('w'), press('.')]);
        assert_eq!(e.buffer.contents(), "new new old");

        // . again on last "old"
        feed(&mut e, &[press('w'), press('.')]);
        assert_eq!(e.buffer.contents(), "new new new");
    }

    // ── Dot-repeat: A (append at end) ───────────────────────────────────

    #[test]
    fn dot_repeat_append_at_end() {
        let mut e = editor_with("hello\nworld");
        // A;  — append semicolon at end of line
        feed(&mut e, &[press('A'), press(';'), esc()]);
        assert_eq!(e.buffer.contents(), "hello;\nworld");

        // j to next line, .
        feed(&mut e, &[press('j'), press('.')]);
        assert_eq!(e.buffer.contents(), "hello;\nworld;");
    }

    // ── Dot-repeat: P (paste before) ────────────────────────────────────

    #[test]
    fn dot_repeat_paste_before() {
        let mut e = editor_with("abcd");
        // x cuts 'a', move to 'd' position, P pastes before
        feed(
            &mut e,
            &[press('x'), press('$'), press('P'), press('.')],
        );
        // x: "bcd" cursor at 'b'. register='a'.
        // $: cursor at 'd' (col 2).
        // P: paste 'a' before col 2 → "bcad". cursor at 'a' (col 2).
        // .: replays P → paste 'a' before col 2 → "bcaad". cursor at 'a' (col 2).
        assert_eq!(e.buffer.contents(), "bcaad");
    }

    // ── Character find: f/F/t/T ─────────────────────────────────────────

    #[test]
    fn f_forward_basic() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('f'), press('w')]);
        assert_eq!(e.cursor.col(), 6);
    }

    #[test]
    fn f_forward_not_found() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('f'), press('z')]);
        assert_eq!(e.cursor.col(), 0); // didn't move
    }

    #[test]
    fn f_forward_with_count() {
        let mut e = editor_with("abracadabra");
        // 3fa → 3rd 'a' after col 0 is at col 7.
        feed(&mut e, &[press('3'), press('f'), press('a')]);
        assert_eq!(e.cursor.col(), 7);
    }

    #[test]
    fn f_backward_basic() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('$')]); // cursor on 'd' (col 10)
        feed(&mut e, &[press('F'), press('o')]);
        assert_eq!(e.cursor.col(), 7);
    }

    #[test]
    fn t_forward_basic() {
        let mut e = editor_with("hello world");
        // tw → one before 'w' at col 6 → lands on col 5 (space).
        feed(&mut e, &[press('t'), press('w')]);
        assert_eq!(e.cursor.col(), 5);
    }

    #[test]
    fn t_forward_adjacent_no_move() {
        let mut e = editor_with("ab");
        // tb → 'b' is adjacent. t lands on col 0 = cursor. No move.
        feed(&mut e, &[press('t'), press('b')]);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn t_backward_basic() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('$')]); // col 10
        // To → 'o' at col 7. T lands on col 8.
        feed(&mut e, &[press('T'), press('o')]);
        assert_eq!(e.cursor.col(), 8);
    }

    // ── Character find with operators: df, dt, cf, ct ────────────────────

    #[test]
    fn df_delete_to_char() {
        let mut e = editor_with("hello.world");
        // dfw → delete from 'h' through 'w' (inclusive). Range [0,7).
        // Wait: f finds 'w' at col 6 (in "world"). Inclusive → [0, 7).
        // Hmm, actually: "hello.world" — 'w' is at col 6.
        // dfw: range [0, 7) → deletes "hello.w" → "orld".
        feed(&mut e, &[press('d'), press('f'), press('w')]);
        assert_eq!(e.buffer.contents(), "orld");
    }

    #[test]
    fn dt_delete_till_char() {
        let mut e = editor_with("hello.world");
        // dtw → t finds 'w' at col 6, lands on col 5 ('.'). Inclusive → [0, 6).
        // Deletes "hello." → "world".
        feed(&mut e, &[press('d'), press('t'), press('w')]);
        assert_eq!(e.buffer.contents(), "world");
    }

    #[test]
    fn df_backward() {
        let mut e = editor_with("hello.world");
        feed(&mut e, &[press('$')]); // col 10 ('d')
        // dFo → F finds 'o' at col 7. Inclusive → [7, 11) → deletes "orld".
        feed(&mut e, &[press('d'), press('F'), press('o')]);
        assert_eq!(e.buffer.contents(), "hello.w");
    }

    #[test]
    fn cf_change_to_char() {
        let mut e = editor_with("hello world");
        // cf<space> → delete "hello " (h through space inclusive), enter insert.
        // Space is at col 5. Range [0, 6). Delete "hello " → "world". Insert.
        feed(
            &mut e,
            &[
                press('c'),
                press('f'),
                press(' '),
                press('H'),
                press('I'),
                press(' '),
                esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "HI world");
    }

    #[test]
    fn df_with_count() {
        let mut e = editor_with("a.b.c.d");
        // d2f. → delete from 'a' through 2nd '.' (col 3). Inclusive → [0, 4).
        feed(
            &mut e,
            &[press('d'), press('2'), press('f'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "c.d");
    }

    #[test]
    fn df_motion_not_found_no_deletion() {
        let mut e = editor_with("hello world");
        // dfz → 'z' not found. No deletion.
        feed(&mut e, &[press('d'), press('f'), press('z')]);
        assert_eq!(e.buffer.contents(), "hello world");
        assert_eq!(e.cursor.col(), 0);
    }

    // ── ; and , repeat ──────────────────────────────────────────────────

    #[test]
    fn semicolon_repeats_forward_find() {
        let mut e = editor_with("abracadabra");
        // fa → col 3 (2nd 'a'). Wait, col 0 is 'a', so fa finds next 'a'.
        // "abracadabra" — a(0) b(1) r(2) a(3) c(4) a(5) d(6) a(7) b(8) r(9) a(10)
        // fa from col 0 → col 3.
        feed(&mut e, &[press('f'), press('a')]);
        assert_eq!(e.cursor.col(), 3);

        // ; → repeats fa, finds next 'a' at col 5.
        feed(&mut e, &[press(';')]);
        assert_eq!(e.cursor.col(), 5);

        // ; → col 7.
        feed(&mut e, &[press(';')]);
        assert_eq!(e.cursor.col(), 7);
    }

    #[test]
    fn comma_repeats_opposite_direction() {
        let mut e = editor_with("abracadabra");
        // fa → col 3. ; → col 5.
        feed(&mut e, &[press('f'), press('a'), press(';')]);
        assert_eq!(e.cursor.col(), 5);

        // , → opposite direction (Fa), finds 'a' backward → col 3.
        feed(&mut e, &[press(',')]);
        assert_eq!(e.cursor.col(), 3);
    }

    #[test]
    fn semicolon_repeats_backward_find() {
        let mut e = editor_with("abracadabra");
        // Move to end, then Fa.
        feed(&mut e, &[press('$')]); // col 10
        feed(&mut e, &[press('F'), press('a')]);
        assert_eq!(e.cursor.col(), 7);

        // ; → repeats Fa backward → col 5.
        feed(&mut e, &[press(';')]);
        assert_eq!(e.cursor.col(), 5);
    }

    #[test]
    fn semicolon_repeats_till() {
        let mut e = editor_with("a.b.c.d");
        // t. → one before first '.', col 0. But wait: cursor at 0, '.' at col 1.
        // t goes one before = col 0 = current pos. No move.
        // Let me use a different starting point.
        // Actually from col 0, the first '.' is at col 1. t. target = col 0 = cursor. No move.
        // Start from col 0: f. first.
        feed(&mut e, &[press('f'), press('.')]); // col 1
        assert_eq!(e.cursor.col(), 1);
        // ; → next '.' at col 3.
        feed(&mut e, &[press(';')]);
        assert_eq!(e.cursor.col(), 3);
    }

    #[test]
    fn semicolon_no_prior_find() {
        let mut e = editor_with("hello world");
        // ; with no prior find — no-op.
        feed(&mut e, &[press(';')]);
        assert_eq!(e.cursor.col(), 0);
    }

    // ── ; and , with operators ──────────────────────────────────────────

    #[test]
    fn d_semicolon_delete_to_repeat() {
        let mut e = editor_with("a.b.c.d");
        // f. → col 1. Then d; deletes from col 1 to next '.' at col 3 (inclusive).
        feed(&mut e, &[press('f'), press('.')]);
        assert_eq!(e.cursor.col(), 1);
        // d; → range [1, 4) → deletes ".b." → "ac.d"
        feed(&mut e, &[press('d'), press(';')]);
        assert_eq!(e.buffer.contents(), "ac.d");
    }

    // ── f/F/t/T in visual mode ──────────────────────────────────────────

    #[test]
    fn vf_extends_selection() {
        let mut e = editor_with("hello world");
        // v enters visual, fw extends selection to 'w'.
        feed(&mut e, &[press('v'), press('f'), press('w')]);
        assert_eq!(e.mode, Mode::Visual(VisualKind::Char));
        assert_eq!(e.cursor.col(), 6);
        assert_eq!(e.cursor.anchor(), Some(Position::ZERO));
    }

    // ── Dot-repeat with character find ───────────────────────────────────

    #[test]
    fn dot_repeat_df() {
        let mut e = editor_with("a.b.c\nx.y.z");
        // df. deletes "a." → "b.c\nx.y.z"
        feed(&mut e, &[press('d'), press('f'), press('.')]);
        assert_eq!(e.buffer.contents(), "b.c\nx.y.z");

        // j goes to next line. . replays df. → deletes "x." → "b.c\ny.z"
        feed(&mut e, &[press('j'), press('.')]);
        assert_eq!(e.buffer.contents(), "b.c\ny.z");
    }

    #[test]
    fn dot_repeat_cf() {
        let mut e = editor_with("(old) (old)");
        // cf) → change from '(' through ')'. Range [0, 5).
        // Deletes "(old)", types "NEW".
        feed(
            &mut e,
            &[
                press('c'),
                press('f'),
                press(')'),
                press('N'),
                press('E'),
                press('W'),
                esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "NEW (old)");

        // Move to '(', . repeats cf) + "NEW" + Esc.
        feed(&mut e, &[press('f'), press('('), press('.')]);
        assert_eq!(e.buffer.contents(), "NEW NEW");
    }

    #[test]
    fn f_escape_cancels() {
        let mut e = editor_with("hello world");
        // f then Escape — no movement, no pending state left.
        feed(&mut e, &[press('f'), esc()]);
        assert_eq!(e.cursor.col(), 0);
        assert!(e.pending.is_none());
    }

    #[test]
    fn df_escape_cancels() {
        let mut e = editor_with("hello world");
        // df then Escape — cancels operator and char find.
        feed(&mut e, &[press('d'), press('f'), esc()]);
        assert_eq!(e.cursor.col(), 0);
        assert_eq!(e.buffer.contents(), "hello world");
    }
}
