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
use n_editor::command::{CmdRange, Command, CommandLine, CommandResult, SubFlags};
use n_editor::cursor::Cursor;
use n_editor::history::History;
use n_editor::jumplist::{ChangeList, JumpList};
use n_editor::mode::{Mode, VisualKind};
use n_editor::position::{Position, Range};
use n_editor::register::{RegisterFile, RegisterKind};
use n_editor::search::{self, SearchDirection, SearchState};
use n_editor::split::{Direction, Rect, Split, WinId};
use n_editor::text_object;
use n_editor::view::{self, View};

use n_term::ansi::CursorShape;
use n_term::buffer::FrameBuffer;
use n_term::event_loop::{Action, App, EventLoop};
use n_term::input::{Event, KeyCode, KeyEvent, Modifiers};
use n_term::terminal::Size;

use regex::Regex;

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
    /// Replace char (`r`). Waiting for the replacement character.
    Replace { count: usize },
    /// `z` key — waiting for second key (`z` = center, `t` = top, `b` = bottom).
    Scroll,
    /// `m` key — waiting for the mark letter (a-z).
    SetMark,
    /// Standalone goto-mark (`` ` `` = exact, `'` = line). Waiting for letter.
    GotoMark { exact: bool },
    /// Operator + goto-mark (`d'a`, `` d`a ``). Waiting for the mark letter.
    OperatorGotoMark { op: char, op_count: usize, exact: bool },
    /// Register selection (`"`). Waiting for the register letter (a-z, A-Z).
    RegisterSelect,
    /// Macro record (`q` when not recording). Waiting for the register letter.
    MacroRecord,
    /// Macro play (`@`). Waiting for the register letter or `@` for repeat.
    MacroPlay { count: usize },
    /// `g` prefix. Waiting for second key: `g` (gg), `;` (changelist back),
    /// `,` (changelist forward).
    GPrefix { count: Option<usize> },
    /// `g` prefix after an operator (`dg`). Waiting for `g` to form `dgg`.
    OperatorGPrefix {
        op: char,
        raw_motion_count: Option<usize>,
    },
    /// `Ctrl+W` prefix — waiting for the window command key (h/j/k/l/w/s/v/c/o).
    CtrlW,
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

// ─── Buffer / window state ─────────────────────────────────────────────────

/// Per-buffer state — the text content and its editing history.
///
/// Stored in `Editor::other_bufs` for inactive buffers. The active buffer
/// lives "unpacked" as flat fields on [`Editor`] for zero-overhead access.
struct BufEntry {
    /// Unique buffer ID (monotonically increasing, Vim-style starting at 1).
    id: usize,
    buffer: Buffer,
    history: History,
    marks: [Option<Position>; 26],
    change_list: ChangeList,
    last_visual_lines: Option<(usize, usize)>,
    /// Last-seen cursor position — restored when a window switches to this buffer.
    last_cursor: Cursor,
    /// Last-seen view state — restored when a window switches to this buffer.
    last_view: View,
}

/// Per-window state — how a window views a buffer.
///
/// Each window has its own cursor and scroll position, independent of other
/// windows that may show the same buffer. Stored in `Editor::other_wins`
/// for inactive windows; the active window's cursor/view are flat fields.
struct WinState {
    /// Unique window ID (monotonically increasing).
    id: WinId,
    /// Which buffer this window is displaying.
    buf_id: usize,
    cursor: Cursor,
    view: View,
}

// ─── Editor ─────────────────────────────────────────────────────────────────

/// The editor application state.
///
#[allow(clippy::struct_excessive_bools)]
///
/// Holds everything needed to edit a file: the text buffer, cursor position,
/// current mode, undo history, view configuration, command line state, and
/// the screen position of the cursor computed during the last paint.
///
/// # Multi-buffer + window architecture
///
/// The active window's buffer state (buffer, history, marks, change list)
/// lives as flat fields. The active window's view state (cursor, view) also
/// lives as flat fields. Inactive buffers are in `other_bufs`, inactive
/// windows in `other_wins`. The split tree describes the window layout.
struct Editor {
    // ── Per-buffer state (active buffer, unpacked) ───────────────────
    buffer: Buffer,
    history: History,

    // ── Per-window state (active window, unpacked) ───────────────────
    cursor: Cursor,
    view: View,
    mode: Mode,

    // ── Multi-buffer management ──────────────────────────────────────

    /// Inactive buffers (text + history + marks). The active buffer lives
    /// unpacked in the flat fields above.
    other_bufs: Vec<BufEntry>,

    /// ID of the current buffer (matches the `id` it would have if packed).
    current_buf_id: usize,

    /// ID of the alternate buffer for `Ctrl+^` quick-switch.
    alternate_buf_id: Option<usize>,

    /// Next ID to assign when creating a new buffer.
    next_buf_id: usize,

    // ── Window management ────────────────────────────────────────────

    /// The split tree describing the window layout.
    split: Split,

    /// ID of the active (focused) window.
    active_win_id: WinId,

    /// Inactive windows. The active window's cursor/view are the flat fields.
    other_wins: Vec<WinState>,

    /// Next window ID to assign.
    next_win_id: WinId,

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

    /// Register file — unnamed + 26 named registers (a-z) for yank/paste.
    registers: RegisterFile,

    /// The register name selected by the `"x` prefix. Consumed by the next
    /// yank, delete, or paste operation. `None` means use the unnamed register.
    selected_register: Option<char>,

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

    /// Height of the text area (rows available for text, excluding status
    /// and command lines) from the last paint. Used by `Ctrl+D`/`Ctrl+U`
    /// to compute half-page scroll distance.
    last_text_height: usize,

    /// The last frame size, used for window navigation layout computation.
    last_frame_size: (u16, u16),

    /// Buffer-local marks (a-z). Each stores the position where `ma`..`mz`
    /// was set. Indexed by `ch - 'a'`.
    marks: [Option<Position>; 26],

    /// Macro key recordings (a-z). Each stores the key sequence recorded
    /// with `qa`..`qz`. Indexed by `ch - 'a'`.
    macro_keys: [Vec<KeyEvent>; 26],

    /// The register index currently being recorded into. `Some(idx)` when
    /// `qa`..`qz` is active, `None` otherwise.
    macro_recording: Option<usize>,

    /// True during `@a` macro replay — prevents recording replayed keys
    /// into the macro register.
    macro_replaying: bool,

    /// The index of the last played macro, for `@@` repeat.
    last_macro: Option<usize>,

    /// Recursion depth during macro replay, to prevent infinite `@a` → `@a`.
    macro_depth: usize,

    /// Last substitution for `:s` repeat and `&`. Stores (pattern, replacement, flags).
    last_sub: Option<(String, String, SubFlags)>,

    /// Last visual selection line range (0-indexed, inclusive) for `'<,'>`.
    /// Stored when leaving visual mode.
    last_visual_lines: Option<(usize, usize)>,

    /// Jump list — position history for `Ctrl+O` / `Ctrl+I` navigation.
    jump_list: JumpList,

    /// Change list — positions where edits occurred, for `g;` / `g,`.
    change_list: ChangeList,
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
            other_bufs: Vec::new(),
            current_buf_id: 1,
            alternate_buf_id: None,
            next_buf_id: 2,
            split: Split::leaf(1),
            active_win_id: 1,
            other_wins: Vec::new(),
            next_win_id: 2,
            pending: None,
            count: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            registers: RegisterFile::new(),
            selected_register: None,
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
            last_text_height: 24, // Sensible default until first paint.
            last_frame_size: (80, 24),
            marks: [None; 26],
            macro_keys: std::array::from_fn(|_| Vec::new()),
            macro_recording: None,
            macro_replaying: false,
            last_macro: None,
            macro_depth: 0,
            last_sub: None,
            last_visual_lines: None,
            jump_list: JumpList::new(),
            change_list: ChangeList::new(),
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
            other_bufs: Vec::new(),
            current_buf_id: 1,
            alternate_buf_id: None,
            next_buf_id: 2,
            split: Split::leaf(1),
            active_win_id: 1,
            other_wins: Vec::new(),
            next_win_id: 2,
            pending: None,
            count: None,
            cursor_screen: None,
            cmdline: CommandLine::new(),
            registers: RegisterFile::new(),
            selected_register: None,
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
            last_text_height: 24,
            last_frame_size: (80, 24),
            marks: [None; 26],
            macro_keys: std::array::from_fn(|_| Vec::new()),
            macro_recording: None,
            macro_replaying: false,
            last_macro: None,
            macro_depth: 0,
            last_sub: None,
            last_visual_lines: None,
            jump_list: JumpList::new(),
            change_list: ChangeList::new(),
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

    /// Commit the current history transaction and record the change position
    /// in the changelist (if the transaction was non-empty).
    fn commit_history(&mut self) {
        if let Some(change_pos) = self.history.commit(self.cursor.position()) {
            self.change_list.push(change_pos);
        }
    }

    // ── Multi-buffer ───────────────────────────────────────────────────

    /// Total number of open buffers (current + other).
    fn buf_count(&self) -> usize {
        1 + self.other_bufs.len()
    }

    /// Format a `buf_info` label for the status line. Returns `""` when there
    /// is only one buffer, otherwise `"[current_id/total]"`.
    fn buf_info_label(&self) -> String {
        if self.other_bufs.is_empty() {
            String::new()
        } else {
            format!("[{}/{}]", self.current_buf_id, self.buf_count())
        }
    }

    // ── Buffer pack/unpack ──────────────────────────────────────────

    /// Pack the active buffer's state into a `BufEntry`.
    fn pack_buf(&mut self) -> BufEntry {
        BufEntry {
            id: self.current_buf_id,
            buffer: std::mem::replace(&mut self.buffer, Buffer::new()),
            history: std::mem::replace(&mut self.history, History::new()),
            marks: std::mem::take(&mut self.marks),
            change_list: std::mem::replace(&mut self.change_list, ChangeList::new()),
            last_visual_lines: self.last_visual_lines.take(),
            last_cursor: self.cursor.clone(),
            last_view: self.view.clone(),
        }
    }

    /// Unpack a `BufEntry` into the active buffer's flat fields.
    fn unpack_buf(&mut self, be: BufEntry) {
        self.current_buf_id = be.id;
        self.buffer = be.buffer;
        self.history = be.history;
        self.marks = be.marks;
        self.change_list = be.change_list;
        self.last_visual_lines = be.last_visual_lines;
    }

    // ── Window pack/unpack ─────────────────────────────────────────

    /// Pack the active window's per-window state.
    const fn pack_win(&mut self) -> WinState {
        WinState {
            id: self.active_win_id,
            buf_id: self.current_buf_id,
            cursor: std::mem::replace(&mut self.cursor, Cursor::new()),
            view: std::mem::replace(&mut self.view, View::new()),
        }
    }

    /// Unpack a `WinState` into the active window's flat fields.
    const fn unpack_win(&mut self, ws: WinState) {
        self.active_win_id = ws.id;
        // Buffer switch handled separately if needed.
        self.cursor = ws.cursor;
        self.view = ws.view;
    }

    /// Switch the active buffer in the current window. Packs/unpacks
    /// the buffer but leaves the window (cursor/view) alone — the caller
    /// is responsible for setting up cursor/view (fresh or restored).
    fn switch_to_buffer(&mut self, target_id: usize) -> bool {
        if target_id == self.current_buf_id {
            return true;
        }

        let Some(target_idx) = self.other_bufs.iter().position(|b| b.id == target_id) else {
            return false;
        };

        // Pack current buffer and swap with target.
        let packed = self.pack_buf();
        let target = std::mem::replace(&mut self.other_bufs[target_idx], packed);
        let old_buf_id = self.other_bufs[target_idx].id;

        // Restore cursor/view from the buffer's last-known state.
        self.cursor = target.last_cursor.clone();
        self.view = target.last_view.clone();
        self.unpack_buf(target);

        // Record alternate for Ctrl+^.
        self.alternate_buf_id = Some(old_buf_id);

        // Reset editing state on buffer switch.
        self.mode = Mode::Normal;
        self.pending = None;
        self.count = None;
        self.search = None;
        self.clear_message();

        true
    }

    // ── Window switching ───────────────────────────────────────────

    /// Switch to a different window by ID.
    fn switch_window(&mut self, target_win_id: WinId) {
        if target_win_id == self.active_win_id {
            return;
        }

        let Some(target_idx) = self.other_wins.iter().position(|w| w.id == target_win_id) else {
            return;
        };

        let target_ws = self.other_wins.remove(target_idx);

        // Pack the active window.
        let packed_win = self.pack_win();
        self.other_wins.push(packed_win);

        // If the target shows a different buffer, swap buffers too.
        if target_ws.buf_id != self.current_buf_id {
            let packed_buf = self.pack_buf();
            self.other_bufs.push(packed_buf);

            let buf_idx = self.other_bufs.iter().position(|b| b.id == target_ws.buf_id).unwrap();
            let target_buf = self.other_bufs.remove(buf_idx);
            self.unpack_buf(target_buf);
        }

        self.unpack_win(target_ws);

        // Reset editing state.
        self.mode = Mode::Normal;
        self.pending = None;
        self.count = None;
        self.search = None;
        self.clear_message();
    }

    /// Sorted list of all buffer IDs (current + other), for ordered iteration.
    fn all_buf_ids_sorted(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = std::iter::once(self.current_buf_id)
            .chain(self.other_bufs.iter().map(|b| b.id))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Open a file in a new buffer. If the file is already open, switch to it.
    fn open_file(&mut self, path: &Path) -> CommandResult {
        // Canonicalize for comparison (ignore errors — use as-is if unresolvable).
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        // Check if already open in current buffer.
        if let Some(cur_path) = self.buffer.path() {
            if std::fs::canonicalize(cur_path).unwrap_or_else(|_| cur_path.to_path_buf()) == canon {
                return CommandResult::Ok(Some(format!(
                    "\"{}\" (already the current buffer)",
                    path.display()
                )));
            }
        }

        // Check if already open in another buffer.
        for bs in &self.other_bufs {
            if let Some(p) = bs.buffer.path() {
                if std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()) == canon {
                    let id = bs.id;
                    self.switch_to_buffer(id);
                    return CommandResult::Ok(Some(format!("\"{}\"", path.display())));
                }
            }
        }

        // Load new file.
        let buf = match Buffer::from_file(path) {
            Ok(b) => b,
            Err(e) => return CommandResult::Err(format!("E325: {e}")),
        };

        // Pack current buffer and store it.
        let packed = self.pack_buf();
        let old_id = packed.id;
        self.other_bufs.push(packed);

        // Set up the new buffer as current.
        let new_id = self.next_buf_id;
        self.next_buf_id += 1;
        self.current_buf_id = new_id;
        self.buffer = buf;
        self.cursor = Cursor::new();
        self.view = View::new();
        self.history = History::new();
        self.marks = [None; 26];
        self.change_list = ChangeList::new();
        self.last_visual_lines = None;

        // Record alternate.
        self.alternate_buf_id = Some(old_id);

        // Reset editing state.
        self.mode = Mode::Normal;
        self.pending = None;
        self.count = None;
        self.search = None;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("???"));
        let lines = self.buffer.line_count();
        CommandResult::Ok(Some(format!("\"{name}\" {lines}L")))
    }

    /// Switch to the next buffer (by ID order). Wraps around.
    fn buf_next(&mut self) -> CommandResult {
        if self.other_bufs.is_empty() {
            return CommandResult::Err("E85: There is no other buffer".to_string());
        }
        let ids = self.all_buf_ids_sorted();
        let cur_pos = ids.iter().position(|&id| id == self.current_buf_id).unwrap();
        let next_id = ids[(cur_pos + 1) % ids.len()];
        self.switch_to_buffer(next_id);
        self.show_buf_switch_message();
        CommandResult::Ok(None)
    }

    /// Switch to the previous buffer (by ID order). Wraps around.
    fn buf_prev(&mut self) -> CommandResult {
        if self.other_bufs.is_empty() {
            return CommandResult::Err("E85: There is no other buffer".to_string());
        }
        let ids = self.all_buf_ids_sorted();
        let cur_pos = ids.iter().position(|&id| id == self.current_buf_id).unwrap();
        let prev_id = ids[(cur_pos + ids.len() - 1) % ids.len()];
        self.switch_to_buffer(prev_id);
        self.show_buf_switch_message();
        CommandResult::Ok(None)
    }

    /// Close the current buffer. If it's the last buffer, quit. If it has
    /// unsaved changes, refuse unless `force` is true.
    fn buf_delete(&mut self, force: bool) -> CommandResult {
        if !force && self.buffer.is_modified() {
            return CommandResult::Err(
                "E89: No write since last change for buffer (add ! to override)".to_string(),
            );
        }

        if self.other_bufs.is_empty() {
            // Last buffer — quit the editor.
            return CommandResult::Quit;
        }

        // Choose which buffer to switch to: alternate if available, else next.
        let target_id = self.alternate_buf_id
            .filter(|&id| self.other_bufs.iter().any(|b| b.id == id))
            .unwrap_or_else(|| {
                // Pick the buffer with the nearest ID.
                let ids = self.all_buf_ids_sorted();
                let cur_pos = ids.iter().position(|&id| id == self.current_buf_id).unwrap();
                if cur_pos + 1 < ids.len() {
                    ids[cur_pos + 1]
                } else {
                    ids[cur_pos.saturating_sub(1)]
                }
            });

        let target_idx = self.other_bufs.iter().position(|b| b.id == target_id).unwrap();
        let target = self.other_bufs.remove(target_idx);
        let old_id = self.current_buf_id;
        self.cursor = target.last_cursor.clone();
        self.view = target.last_view.clone();
        self.unpack_buf(target);

        // Set alternate to the closest remaining buffer (not the deleted one).
        self.alternate_buf_id = if self.other_bufs.is_empty() {
            None
        } else {
            // Find the buffer that was alternate before, if it's still alive.
            Some(old_id).filter(|_| false) // old buffer is deleted, can't be alternate
                .or_else(|| self.other_bufs.first().map(|b| b.id))
        };

        // Reset editing state.
        self.mode = Mode::Normal;
        self.pending = None;
        self.count = None;
        self.search = None;

        self.show_buf_switch_message();
        CommandResult::Ok(None)
    }

    /// Build the `:ls` buffer listing.
    fn buf_list(&self) -> String {
        let ids = self.all_buf_ids_sorted();
        let mut lines = Vec::with_capacity(ids.len());
        for &id in &ids {
            if id == self.current_buf_id {
                let name = self.buffer.path()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("[No Name]");
                let modified = if self.buffer.is_modified() { "+" } else { "" };
                let alt = if self.alternate_buf_id == Some(id) { "#" } else { "" };
                lines.push(format!("  {id:>3} %a{alt} \"{name}\" {modified}"));
            } else if let Some(bs) = self.other_bufs.iter().find(|b| b.id == id) {
                let name = bs.buffer.path()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("[No Name]");
                let modified = if bs.buffer.is_modified() { "+" } else { "" };
                let alt = if self.alternate_buf_id == Some(id) { "#" } else { "" };
                let current = "";
                lines.push(format!("  {id:>3} {current}{alt} \"{name}\" {modified}"));
            }
        }
        lines.join("\n")
    }

    /// Set the message to show after a buffer switch.
    fn show_buf_switch_message(&mut self) {
        let name = self.buffer.path()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[No Name]");
        let lines = self.buffer.line_count();
        self.set_message(format!("\"{name}\" {lines}L"));
    }

    // ── Window management ──────────────────────────────────────────────

    /// Total number of windows.
    fn win_count(&self) -> usize {
        self.split.window_count()
    }

    /// `:sp` — split the current window horizontally (top/bottom).
    fn win_split_horizontal(&mut self) -> CommandResult {
        let new_win_id = self.next_win_id;
        self.next_win_id += 1;

        // The new window gets a clone of the current cursor/view and
        // references the same buffer.
        let new_win = WinState {
            id: new_win_id,
            buf_id: self.current_buf_id,
            cursor: self.cursor.clone(),
            view: self.view.clone(),
        };
        self.other_wins.push(new_win);
        self.split.split_horizontal(self.active_win_id, new_win_id);
        CommandResult::Ok(None)
    }

    /// `:vsp` — split the current window vertically (left/right).
    fn win_split_vertical(&mut self) -> CommandResult {
        let new_win_id = self.next_win_id;
        self.next_win_id += 1;

        let new_win = WinState {
            id: new_win_id,
            buf_id: self.current_buf_id,
            cursor: self.cursor.clone(),
            view: self.view.clone(),
        };
        self.other_wins.push(new_win);
        self.split.split_vertical(self.active_win_id, new_win_id);
        CommandResult::Ok(None)
    }

    /// `:close` — close the current window (buffer stays open).
    fn win_close(&mut self) -> CommandResult {
        if self.win_count() <= 1 {
            return CommandResult::Err(
                "E444: Cannot close last window".to_string(),
            );
        }

        // Find the next window to switch to.
        let next_id = self.split.cycle_next(self.active_win_id);
        self.split.remove(self.active_win_id);

        // Load the target window's state.
        let target_idx = self.other_wins.iter().position(|w| w.id == next_id).unwrap();
        let target_ws = self.other_wins.remove(target_idx);

        // Switch buffer if needed.
        if target_ws.buf_id != self.current_buf_id {
            self.pack_and_swap_buf(target_ws.buf_id);
        }

        self.unpack_win(target_ws);

        // Reset editing state.
        self.mode = Mode::Normal;
        self.pending = None;
        self.count = None;
        self.search = None;
        self.clear_message();
        CommandResult::Ok(None)
    }

    /// `:only` — close all windows except the current one.
    fn win_only(&mut self) -> CommandResult {
        if self.win_count() <= 1 {
            return CommandResult::Ok(None); // Already the only window.
        }

        let removed = self.split.keep_only(self.active_win_id);
        // Remove all inactive window states for the closed windows.
        self.other_wins.retain(|w| !removed.contains(&w.id));
        CommandResult::Ok(None)
    }

    /// Pack the current buffer and load a different one by ID.
    fn pack_and_swap_buf(&mut self, target_buf_id: usize) {
        if target_buf_id == self.current_buf_id {
            return;
        }
        let packed = self.pack_buf();
        self.other_bufs.push(packed);
        let idx = self.other_bufs.iter().position(|b| b.id == target_buf_id).unwrap();
        let target = self.other_bufs.remove(idx);
        self.unpack_buf(target);
    }

    /// Get a reference to a buffer by ID (active or inactive).
    fn get_buffer_by_id(&self, buf_id: usize) -> &Buffer {
        if buf_id == self.current_buf_id {
            &self.buffer
        } else {
            &self.other_bufs.iter().find(|b| b.id == buf_id).unwrap().buffer
        }
    }

    /// Render an inactive window into its rectangle.
    ///
    /// Temporarily removes the `WinState` from `other_wins` to avoid
    /// borrow conflicts (we need `&mut view` and `&buffer` simultaneously).
    fn render_inactive_window(
        &mut self,
        win_id: WinId,
        buf_info: &str,
        frame: &mut FrameBuffer,
        rect: Rect,
    ) {
        let Some(ws_idx) = self.other_wins.iter().position(|w| w.id == win_id) else {
            return;
        };

        // Temporarily take the WinState out so we can borrow self.buffer
        // and ws.view mutably without conflict.
        let mut ws = self.other_wins.remove(ws_idx);
        let buf = self.get_buffer_by_id(ws.buf_id);
        ws.view.render(
            buf, &ws.cursor, Mode::Normal, None, buf_info,
            frame, rect.x, rect.y, rect.w, rect.h,
        );
        self.other_wins.insert(ws_idx, ws);
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

    // ── Macro replay ────────────────────────────────────────────────────

    /// Maximum macro recursion depth (prevents infinite `@a` → `@a` loops).
    const MAX_MACRO_DEPTH: usize = 100;

    /// Replay a macro from register `idx` (0-25), `count` times.
    fn macro_replay(&mut self, idx: usize, count: usize) -> Action {
        if idx >= 26 || self.macro_keys[idx].is_empty() {
            return Action::Continue;
        }

        if self.macro_depth >= Self::MAX_MACRO_DEPTH {
            self.set_error("E132: macro recursion too deep");
            return Action::Continue;
        }

        self.last_macro = Some(idx);
        self.macro_depth += 1;
        let was_replaying = self.macro_replaying;
        self.macro_replaying = true;

        for _ in 0..count {
            let keys = self.macro_keys[idx].clone();
            for key in &keys {
                let event = Event::Key(*key);
                let action = self.on_event(&event);
                if matches!(action, Action::Quit) {
                    self.macro_replaying = was_replaying;
                    self.macro_depth -= 1;
                    return Action::Quit;
                }
            }
        }

        self.macro_replaying = was_replaying;
        self.macro_depth -= 1;
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

            // File motion: G — jump (pushes to jump list)
            // Note: `g` (gg) is now a prefix key handled via Pending::GPrefix.
            KeyCode::Char('G') => {
                self.jump_list.push(self.cursor.position());
                if let Some(n) = raw_count {
                    self.cursor.goto_line(n.saturating_sub(1), &self.buffer, pe);
                } else {
                    self.cursor.move_to_last_line(&self.buffer, pe);
                }
            }

            // Paragraph motions — jumps (push to jump list)
            KeyCode::Char('}') => {
                self.jump_list.push(self.cursor.position());
                self.cursor.paragraph_forward(count, &self.buffer, pe);
            }
            KeyCode::Char('{') => {
                self.jump_list.push(self.cursor.position());
                self.cursor.paragraph_backward(count, &self.buffer, pe);
            }

            // Matching bracket — jump (pushes to jump list)
            KeyCode::Char('%') => {
                if let Some(pos) = find_matching_bracket(&self.buffer, self.cursor.position()) {
                    self.jump_list.push(self.cursor.position());
                    self.cursor.set_position(pos, &self.buffer, pe);
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

    #[allow(clippy::too_many_lines)]
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
                KeyCode::Char('d') => {
                    self.pending = None;
                    let count = self.take_count();
                    self.scroll_half_page_down(count);
                    return Action::Continue;
                }
                KeyCode::Char('u') => {
                    self.pending = None;
                    let count = self.take_count();
                    self.scroll_half_page_up(count);
                    return Action::Continue;
                }
                KeyCode::Char('^' | '6') => {
                    // Ctrl+^ (or Ctrl+6) — switch to alternate buffer.
                    self.pending = None;
                    self.count = None;
                    if let Some(alt_id) = self.alternate_buf_id {
                        if self.switch_to_buffer(alt_id) {
                            self.show_buf_switch_message();
                        } else {
                            self.set_error("E23: No alternate file");
                        }
                    } else {
                        self.set_error("E23: No alternate file");
                    }
                    return Action::Continue;
                }
                KeyCode::Char('o') => {
                    // Ctrl+O — jump backward through the jump list.
                    self.pending = None;
                    let count = self.take_count();
                    let mut last_pos = None;
                    for _ in 0..count {
                        if let Some(pos) = self.jump_list.back(self.cursor.position()) {
                            self.cursor.set_position(pos, &self.buffer, pe);
                            last_pos = Some(pos);
                        } else {
                            break;
                        }
                    }
                    if last_pos.is_none() {
                        // Already at the start of the jump list — no bell,
                        // but we could show a message if desired.
                    }
                    return Action::Continue;
                }
                KeyCode::Char('w') => {
                    // Ctrl+W — window command prefix.
                    self.pending = Some(Pending::CtrlW);
                    self.count = None;
                    return Action::Continue;
                }
                _ => {}
            }
        }

        // Tab = Ctrl+I — jump forward through the jump list.
        if key.code == KeyCode::Tab && !key.modifiers.contains(Modifiers::SHIFT) {
            self.pending = None;
            let count = self.take_count();
            for _ in 0..count {
                if let Some(pos) = self.jump_list.forward() {
                    self.cursor.set_position(pos, &self.buffer, pe);
                } else {
                    break;
                }
            }
            return Action::Continue;
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

        // Register prefix: `"` selects a register for the next operation.
        // Transparent to count — the count passes through to the command.
        if key.code == KeyCode::Char('"') {
            self.pending = Some(Pending::RegisterSelect);
            return Action::Continue;
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

                // Same key = line operation (dd, yy, cc, >>, <<).
                // Effective count: op_count * motion_count.
                if key.code == KeyCode::Char(op) {
                    let raw_motion_count = self.take_raw_count();
                    let motion_count = raw_motion_count.unwrap_or(1);
                    let effective = op_count * motion_count;

                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }

                    let action = if op == '>' || op == '<' {
                        self.indent_outdent_line_op(op, effective);
                        Action::Continue
                    } else {
                        self.operator_line(op, effective)
                    };

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

                // Goto-mark prefix: ` and ' need one more key (the mark letter).
                if key.code == KeyCode::Char('`') || key.code == KeyCode::Char('\'') {
                    let exact = key.code == KeyCode::Char('`');
                    // Record this key for dot-repeat.
                    if self.dot_recording && !self.dot_replaying {
                        self.dot_keys.push(*key);
                    }
                    self.pending = Some(Pending::OperatorGotoMark {
                        op,
                        op_count,
                        exact,
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
                            let action = self.execute_operator(op, range, false);
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

                // `g` prefix — need a second key for `gg` motion.
                if key.code == KeyCode::Char('g') {
                    let raw_motion_count = self.take_raw_count();
                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }
                    self.pending = Some(Pending::OperatorGPrefix {
                        op,
                        raw_motion_count,
                    });
                    return Action::Continue;
                }

                // Try as a motion. The motion's own count multiplies with
                // the operator count, except for G where it's a line number.
                let raw_motion_count = self.take_raw_count();
                let effective = op_count * raw_motion_count.unwrap_or(1);
                if let Some(range) =
                    self.operator_motion_range(key.code, op, effective, raw_motion_count)
                {
                    if self.dot_recording && !self.dot_replaying {
                        self.dot_effective_count =
                            Self::merge_counts(self.dot_effective_count, raw_motion_count);
                    }

                    let action = self.execute_operator(op, range, false);

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
                    let action = self.execute_operator(op, range, false);

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
            Pending::Replace { count } => {
                // `r` + char: replace `count` characters under the cursor.
                if key.code == KeyCode::Escape {
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Record the replacement char for dot-repeat.
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                if let KeyCode::Char(ch) = key.code {
                    self.replace_chars(ch, count);
                    self.dot_finish();
                } else {
                    self.dot_cancel();
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
                        let action = self.execute_operator(op, range, false);
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
            Pending::Scroll => {
                // `z` + second key: scroll positioning.
                match key.code {
                    KeyCode::Char('z') => self.scroll_cursor_center(),
                    KeyCode::Char('t') | KeyCode::Enter => self.scroll_cursor_top(),
                    KeyCode::Char('b') => self.scroll_cursor_bottom(),
                    _ => {} // Unrecognized — cancel silently.
                }
                Action::Continue
            }
            Pending::SetMark => {
                // `m` + letter: set a mark at the current position.
                if let KeyCode::Char(ch @ 'a'..='z') = key.code {
                    self.marks[(ch as u8 - b'a') as usize] = Some(self.cursor.position());
                }
                // Non-letter or Escape — cancel silently.
                Action::Continue
            }
            Pending::GotoMark { exact } => {
                // `` `a `` or `'a`: jump to mark (pushes to jump list).
                if let KeyCode::Char(ch @ 'a'..='z') = key.code {
                    self.jump_list.push(self.cursor.position());
                    self.goto_mark(ch, exact);
                }
                // Non-letter or Escape — cancel silently.
                Action::Continue
            }
            Pending::OperatorGotoMark { op, op_count, exact } => {
                // `d'a`, `` d`a ``: operator to mark position.
                if key.code == KeyCode::Escape {
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Record mark key for dot-repeat.
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                if let KeyCode::Char(ch @ 'a'..='z') = key.code {
                    if let Some(range) = self.mark_operator_range(ch, exact, op_count) {
                        let linewise = !exact; // 'a is linewise, `a is charwise
                        let action = self.execute_operator(op, range, linewise);
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
            Pending::RegisterSelect => {
                // `"` + register letter: select a register for the next operation.
                if let KeyCode::Char(ch @ ('a'..='z' | 'A'..='Z')) = key.code {
                    self.selected_register = Some(ch);
                }
                // Escape or unrecognized key — cancel silently.
                Action::Continue
            }
            Pending::MacroRecord => {
                // `q` + register letter: start recording a macro.
                if let KeyCode::Char(ch @ 'a'..='z') = key.code {
                    let idx = (ch as u8 - b'a') as usize;
                    self.macro_keys[idx].clear();
                    self.macro_recording = Some(idx);
                }
                // Escape or non-letter — cancel silently.
                Action::Continue
            }
            Pending::MacroPlay { count } => {
                match key.code {
                    // `@@` — replay the last played macro.
                    KeyCode::Char('@') => {
                        if let Some(idx) = self.last_macro {
                            return self.macro_replay(idx, count);
                        }
                    }
                    // `@a`..`@z` — replay macro from that register.
                    KeyCode::Char(ch @ 'a'..='z') => {
                        let idx = (ch as u8 - b'a') as usize;
                        return self.macro_replay(idx, count);
                    }
                    _ => {} // Escape or unrecognized — cancel silently.
                }
                Action::Continue
            }
            Pending::GPrefix { count } => {
                let pe = self.mode.cursor_past_end();
                match key.code {
                    KeyCode::Char('g') => {
                        // `gg` — goto first line (or Nth line with count).
                        self.jump_list.push(self.cursor.position());
                        if let Some(n) = count {
                            self.cursor
                                .goto_line(n.saturating_sub(1), &self.buffer, pe);
                        } else {
                            self.cursor.move_to_first_line(&self.buffer, pe);
                        }
                    }
                    KeyCode::Char(';') => {
                        // `g;` — jump to older change position.
                        let n = count.unwrap_or(1);
                        for _ in 0..n {
                            if let Some(pos) = self.change_list.back() {
                                self.cursor.set_position(pos, &self.buffer, pe);
                            } else {
                                self.set_error("E662: At start of changelist");
                                break;
                            }
                        }
                    }
                    KeyCode::Char(',') => {
                        // `g,` — jump to newer change position.
                        let n = count.unwrap_or(1);
                        for _ in 0..n {
                            if let Some(pos) = self.change_list.forward() {
                                self.cursor.set_position(pos, &self.buffer, pe);
                            } else {
                                self.set_error("E663: At end of changelist");
                                break;
                            }
                        }
                    }
                    _ => {} // Unrecognized — cancel silently.
                }
                Action::Continue
            }
            Pending::OperatorGPrefix {
                op,
                raw_motion_count,
            } => {
                // `dgg`, `cgg`, `ygg` — operator to first/Nth line.
                if key.code == KeyCode::Escape {
                    self.dot_cancel();
                    return Action::Continue;
                }

                // Record this key for dot-repeat.
                if self.dot_recording && !self.dot_replaying {
                    self.dot_keys.push(*key);
                }

                if key.code == KeyCode::Char('g') {
                    let start = self.cursor.position();
                    let mut c = self.cursor.clone();
                    if let Some(n) = raw_motion_count {
                        c.goto_line(n.saturating_sub(1), &self.buffer, false);
                    } else {
                        c.move_to_first_line(&self.buffer, false);
                    }
                    if let Some(range) = self.linewise_range(start, c.position()) {
                        let action = self.execute_operator(op, range, true);
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
            Pending::CtrlW => {
                match key.code {
                    KeyCode::Char('w') => {
                        // Ctrl+W w — cycle to next window.
                        let next = self.split.cycle_next(self.active_win_id);
                        self.switch_window(next);
                    }
                    KeyCode::Char('W') => {
                        // Ctrl+W W — cycle to previous window.
                        let prev = self.split.cycle_prev(self.active_win_id);
                        self.switch_window(prev);
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        self.win_navigate(Direction::Left);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.win_navigate(Direction::Down);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.win_navigate(Direction::Up);
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        self.win_navigate(Direction::Right);
                    }
                    KeyCode::Char('s') => {
                        // Ctrl+W s — same as :sp.
                        self.win_split_horizontal();
                    }
                    KeyCode::Char('v') => {
                        // Ctrl+W v — same as :vsp.
                        self.win_split_vertical();
                    }
                    KeyCode::Char('c') => {
                        // Ctrl+W c — close current window.
                        if let CommandResult::Err(msg) = self.win_close() {
                            self.set_error(msg);
                        }
                    }
                    KeyCode::Char('o') => {
                        // Ctrl+W o — close all other windows.
                        self.win_only();
                    }
                    _ => {} // Unrecognized or Escape — cancel silently.
                }
                Action::Continue
            }
        }
    }

    /// Navigate to the window in the given direction using the split layout.
    fn win_navigate(&mut self, dir: Direction) {
        let (w, h) = self.last_frame_size;
        let main_h = h.saturating_sub(1); // exclude command line row
        let area = Rect { x: 0, y: 0, w, h: main_h };
        if let Some(target) = self.split.neighbor(self.active_win_id, dir, area) {
            self.switch_window(target);
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
            KeyCode::Char('>') => {
                self.dot_start(key, raw_count);
                self.pending = Some(Pending::Operator { op: '>', count });
            }
            KeyCode::Char('<') => {
                self.dot_start(key, raw_count);
                self.pending = Some(Pending::Operator { op: '<', count });
            }
            KeyCode::Char('x') => {
                self.dot_immediate(key, raw_count);
                self.delete_chars_at_cursor(count);
            }

            // -- Shortcuts (C = c$, D = d$, S = cc) --
            KeyCode::Char('D') => {
                self.dot_immediate(key, raw_count);
                let pos = self.cursor.position();
                let target_line =
                    (pos.line + count - 1).min(self.buffer.line_count().saturating_sub(1));
                let target_len = self.buffer.line_content_len(target_line).unwrap_or(0);
                let end = Position::new(target_line, target_len);
                if pos < end {
                    let range = Range::new(pos, end);
                    self.apply_operator('d', range, false);
                }
            }
            KeyCode::Char('C') => {
                self.dot_start(key, raw_count);
                let pos = self.cursor.position();
                let target_line =
                    (pos.line + count - 1).min(self.buffer.line_count().saturating_sub(1));
                let target_len = self.buffer.line_content_len(target_line).unwrap_or(0);
                let end = Position::new(target_line, target_len);
                if pos < end {
                    let range = Range::new(pos, end);
                    self.apply_operator('c', range, false);
                } else {
                    // At end of line — just enter insert mode.
                    self.history.begin(pos);
                    self.mode = Mode::Insert;
                }
            }
            KeyCode::Char('S') => {
                self.dot_start(key, raw_count);
                self.operator_line('c', count);
            }

            // -- Join lines --
            KeyCode::Char('J') => {
                self.dot_immediate(key, raw_count);
                self.join_lines(count);
            }

            // -- Toggle case --
            KeyCode::Char('~') => {
                self.dot_immediate(key, raw_count);
                self.toggle_case(count);
            }

            // -- Replace char (enter pending, waiting for replacement) --
            KeyCode::Char('r') => {
                self.dot_start(key, raw_count);
                self.pending = Some(Pending::Replace { count });
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

            // -- Repeat last substitution --
            KeyCode::Char('&') => {
                let result = self.cmd_sub_repeat(&CmdRange::CurrentLine);
                match result {
                    CommandResult::Ok(Some(msg)) => self.set_message(msg),
                    CommandResult::Ok(None) | CommandResult::Quit => {}
                    CommandResult::Err(msg) => self.set_error(msg),
                }
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

            // -- Scroll positioning (z + z/t/b) --
            KeyCode::Char('z') => {
                self.pending = Some(Pending::Scroll);
            }

            // -- Marks --
            KeyCode::Char('m') => {
                self.pending = Some(Pending::SetMark);
            }
            KeyCode::Char('`') => {
                self.pending = Some(Pending::GotoMark { exact: true });
            }
            KeyCode::Char('\'') => {
                self.pending = Some(Pending::GotoMark { exact: false });
            }

            // -- g prefix (gg, g;, g,) --
            KeyCode::Char('g') => {
                self.pending = Some(Pending::GPrefix { count: raw_count });
            }

            // -- Search (all are jump motions) --
            KeyCode::Char('/') => self.start_search(SearchDirection::Forward),
            KeyCode::Char('?') => self.start_search(SearchDirection::Backward),
            KeyCode::Char('n') => {
                self.jump_list.push(self.cursor.position());
                for _ in 0..count {
                    self.search_next();
                }
            }
            KeyCode::Char('N') => {
                self.jump_list.push(self.cursor.position());
                for _ in 0..count {
                    self.search_prev();
                }
            }
            KeyCode::Char('*') => {
                self.jump_list.push(self.cursor.position());
                self.search_word_under_cursor(SearchDirection::Forward);
            }
            KeyCode::Char('#') => {
                self.jump_list.push(self.cursor.position());
                self.search_word_under_cursor(SearchDirection::Backward);
            }

            // -- Macro record (q + register) --
            KeyCode::Char('q') => {
                // Don't allow starting a recording during macro replay.
                if !self.macro_replaying {
                    self.pending = Some(Pending::MacroRecord);
                }
            }

            // -- Macro play (@ + register) --
            KeyCode::Char('@') => {
                self.pending = Some(Pending::MacroPlay { count });
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
            // Note: `g` (gg) is handled via Pending::OperatorGPrefix.

            // Paragraph motions — linewise when used with operators.
            KeyCode::Char('}') => {
                c.paragraph_forward(effective, &self.buffer, false);
                return self.linewise_range(start, c.position());
            }
            KeyCode::Char('{') => {
                c.paragraph_backward(effective, &self.buffer, false);
                return self.linewise_range(start, c.position());
            }

            // Matching bracket — inclusive motion.
            KeyCode::Char('%') => {
                if let Some(pos) = find_matching_bracket(&self.buffer, start) {
                    c.set_position(pos, &self.buffer, false);
                    true
                } else {
                    return None;
                }
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

        // Consume the register selection (set by `"x` prefix).
        let reg_name = self.selected_register.take();

        match op {
            'd' => {
                self.registers.yank(reg_name, reg_text, reg_kind);
                self.history.begin(self.cursor.position());
                self.history.record_delete(range.start, &text);
                self.buffer.delete(range);
                self.cursor
                    .set_position(range.start, &self.buffer, false);
                self.cursor.clamp(&self.buffer, false);
                if linewise {
                    self.cursor.move_to_first_non_blank(&self.buffer, false);
                }
                self.commit_history();
            }
            'c' => {
                self.registers.yank(reg_name, reg_text, reg_kind);
                self.history.begin(self.cursor.position());
                self.history.record_delete(range.start, &text);
                self.buffer.delete(range);
                self.cursor
                    .set_position(range.start, &self.buffer, true);
                self.cursor.clamp(&self.buffer, true);
                self.commit_history();
                // Begin a new transaction for the insert session.
                self.history.begin(self.cursor.position());
                self.mode = Mode::Insert;
            }
            'y' => {
                self.registers.yank(reg_name, reg_text, reg_kind);
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
                self.commit_history();
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
            Command::Edit(path) => self.open_file(&path),
            Command::BufNext => self.buf_next(),
            Command::BufPrev => self.buf_prev(),
            Command::BufDelete => self.buf_delete(false),
            Command::BufDeleteForce => self.buf_delete(true),
            Command::BufList => {
                let listing = self.buf_list();
                CommandResult::Ok(Some(listing))
            }
            Command::Substitute { range, pattern, replacement, flags } => {
                self.cmd_substitute(&range, &pattern, &replacement, flags)
            }
            Command::SubRepeat { range } => self.cmd_sub_repeat(&range),
            Command::Split => self.win_split_horizontal(),
            Command::VSplit => self.win_split_vertical(),
            Command::WinClose => self.win_close(),
            Command::WinOnly => self.win_only(),
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

    /// `:q` — quit if no unsaved changes in any buffer.
    fn cmd_quit(&self) -> CommandResult {
        if self.buffer.is_modified() {
            return CommandResult::Err(
                "E37: No write since last change (add ! to override)".to_string(),
            );
        }
        if let Some(bs) = self.other_bufs.iter().find(|b| b.buffer.is_modified()) {
            let name = bs.buffer.path()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("[No Name]");
            return CommandResult::Err(
                format!("E37: No write since last change for buffer \"{name}\" (add ! to override)")
            );
        }
        CommandResult::Quit
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

    // ── Substitution ────────────────────────────────────────────────────

    /// `:[range]s/pattern/replacement/flags` — find and replace.
    fn cmd_substitute(
        &mut self,
        range: &CmdRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> CommandResult {
        if pattern.is_empty() {
            return CommandResult::Err("E486: Pattern is empty".to_string());
        }

        // Store for `:s` repeat and `&`.
        self.last_sub = Some((pattern.to_string(), replacement.to_string(), flags));

        self.execute_substitute(range, pattern, replacement, flags)
    }

    /// `:s` (no args) — repeat last substitution.
    fn cmd_sub_repeat(&mut self, range: &CmdRange) -> CommandResult {
        let Some((pattern, replacement, flags)) = self.last_sub.clone() else {
            return CommandResult::Err("E33: No previous substitute regular expression".to_string());
        };
        self.execute_substitute(range, &pattern, &replacement, flags)
    }

    /// Core substitution engine shared by `:s/pat/rep/flags` and `:s` repeat.
    fn execute_substitute(
        &mut self,
        range: &CmdRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> CommandResult {
        // Resolve the line range.
        let (first, last) = match self.resolve_range(range) {
            Ok(r) => r,
            Err(msg) => return CommandResult::Err(msg),
        };

        // Clamp to buffer.
        let line_count = self.buffer.line_count();
        if line_count == 0 {
            return CommandResult::Err("E486: Pattern not found: ".to_string() + pattern);
        }
        let last = last.min(line_count - 1);
        if first > last {
            return CommandResult::Err("E486: Pattern not found: ".to_string() + pattern);
        }

        // Compile the regex.
        let regex_pattern = if flags.case_insensitive {
            format!("(?i){pattern}")
        } else {
            pattern.to_string()
        };
        let re = match Regex::new(&regex_pattern) {
            Ok(r) => r,
            Err(e) => return CommandResult::Err(format!("E486: Invalid pattern: {e}")),
        };

        // Translate Vim-style replacement to regex-crate syntax.
        let rep = translate_replacement(replacement);

        // Perform the substitution.
        let mut total_subs: usize = 0;
        let mut total_lines: usize = 0;

        if flags.count_only {
            // `n` flag: count matches without replacing.
            for line_idx in first..=last {
                let content = self.line_content(line_idx);
                let count = if flags.global {
                    re.find_iter(&content).count()
                } else {
                    usize::from(re.is_match(&content))
                };
                if count > 0 {
                    total_subs += count;
                    total_lines += 1;
                }
            }

            return if total_subs > 0 {
                CommandResult::Ok(Some(format!(
                    "{total_subs} match{} on {total_lines} line{}",
                    if total_subs == 1 { "" } else { "es" },
                    if total_lines == 1 { "" } else { "s" },
                )))
            } else {
                CommandResult::Err("E486: Pattern not found: ".to_string() + pattern)
            };
        }

        // Replacing: iterate backwards so line positions stay valid.
        self.history.begin(self.cursor.position());

        for line_idx in (first..=last).rev() {
            let content = self.line_content(line_idx);
            let new_content = if flags.global {
                re.replace_all(&content, rep.as_str()).into_owned()
            } else {
                re.replace(&content, rep.as_str()).into_owned()
            };

            if new_content != content {
                let count = if flags.global {
                    re.find_iter(&content).count()
                } else {
                    1
                };
                total_subs += count;
                total_lines += 1;

                // Replace the line content in the buffer.
                let content_len = self.buffer.line_content_len(line_idx).unwrap_or(0);
                let start = Position::new(line_idx, 0);
                let end = Position::new(line_idx, content_len);
                let range = Range::new(start, end);

                self.history.record_delete(start, &content);
                self.buffer.delete(range);
                self.history.record_insert(start, &new_content);
                self.buffer.insert(start, &new_content);
            }
        }

        // Place cursor on the first line of the range (Vim behavior).
        if total_subs > 0 {
            self.cursor
                .set_position(Position::new(first, 0), &self.buffer, false);
            self.cursor.move_to_first_non_blank(&self.buffer, false);
        }

        self.commit_history();

        if total_subs > 0 {
            if total_lines > 1 || total_subs > 1 {
                CommandResult::Ok(Some(format!(
                    "{total_subs} substitution{} on {total_lines} line{}",
                    if total_subs == 1 { "" } else { "s" },
                    if total_lines == 1 { "" } else { "s" },
                )))
            } else {
                CommandResult::Ok(None)
            }
        } else {
            // No history to commit if nothing changed.
            CommandResult::Err("E486: Pattern not found: ".to_string() + pattern)
        }
    }

    /// Resolve a [`CmdRange`] to an inclusive line range `(first, last)`.
    fn resolve_range(&self, range: &CmdRange) -> Result<(usize, usize), String> {
        match range {
            CmdRange::CurrentLine => {
                let line = self.cursor.position().line;
                Ok((line, line))
            }
            CmdRange::All => {
                let last = self.buffer.line_count().saturating_sub(1);
                Ok((0, last))
            }
            CmdRange::Lines(start, end) => {
                if *start > *end {
                    return Err("E493: Backwards range given".to_string());
                }
                Ok((*start, *end))
            }
            CmdRange::Visual => {
                if let Some((start, end)) = self.last_visual_lines {
                    Ok((start, end))
                } else {
                    Err("E20: Mark not set".to_string())
                }
            }
        }
    }

    /// Get the content of a line (without trailing newline) as a `String`.
    fn line_content(&self, line_idx: usize) -> String {
        self.buffer
            .line(line_idx)
            .map(|rope_slice| {
                let s: String = rope_slice.chars().collect();
                s.trim_end_matches(['\n', '\r']).to_string()
            })
            .unwrap_or_default()
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
            let count = self.take_count();
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
                KeyCode::Char('d') => {
                    self.scroll_half_page_down(count);
                    return Action::Continue;
                }
                KeyCode::Char('u') => {
                    self.scroll_half_page_up(count);
                    return Action::Continue;
                }
                _ => {}
            }
        }

        // Handle pending state (f/F/t/T, goto-mark, scroll).
        if let Some(pending) = self.pending.take() {
            match pending {
                Pending::CharFind { kind, count } => {
                    if let KeyCode::Char(ch) = key.code {
                        self.last_char_find = Some((ch, kind));
                        self.execute_char_find_motion(ch, kind, count, pe);
                    }
                }
                Pending::GotoMark { exact } => {
                    if let KeyCode::Char(ch @ 'a'..='z') = key.code {
                        self.jump_list.push(self.cursor.position());
                        self.goto_mark(ch, exact);
                    }
                }
                Pending::GPrefix { count } => {
                    if key.code == KeyCode::Char('g') {
                        // `gg` — goto first/Nth line.
                        self.jump_list.push(self.cursor.position());
                        if let Some(n) = count {
                            self.cursor
                                .goto_line(n.saturating_sub(1), &self.buffer, pe);
                        } else {
                            self.cursor.move_to_first_line(&self.buffer, pe);
                        }
                    }
                    // g; and g, are not valid in visual mode — cancel.
                }
                Pending::Scroll => {
                    match key.code {
                        KeyCode::Char('z') => self.scroll_cursor_center(),
                        KeyCode::Char('t') | KeyCode::Enter => self.scroll_cursor_top(),
                        KeyCode::Char('b') => self.scroll_cursor_bottom(),
                        _ => {}
                    }
                }
                Pending::RegisterSelect => {
                    if let KeyCode::Char(ch @ ('a'..='z' | 'A'..='Z')) = key.code {
                        self.selected_register = Some(ch);
                    }
                }
                _ => {} // Other pending types cancel silently.
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

            // -- Enter command mode (prefill with '<,'>) --
            KeyCode::Char(':') => {
                self.save_visual_lines();
                self.cursor.clear_anchor();
                self.mode = Mode::Command;
                self.cmdline.clear();
                // Prefill with the visual range markers (like Vim).
                for ch in "'<,'>".chars() {
                    self.cmdline.insert_char(ch);
                }
            }

            // -- Indent / outdent --
            KeyCode::Char('>') => self.visual_indent(),
            KeyCode::Char('<') => self.visual_outdent(),

            // -- Scroll positioning --
            KeyCode::Char('z') => {
                self.pending = Some(Pending::Scroll);
            }

            // -- Register selection --
            KeyCode::Char('"') => {
                self.pending = Some(Pending::RegisterSelect);
            }

            // -- g prefix (gg) --
            KeyCode::Char('g') => {
                self.pending = Some(Pending::GPrefix { count: raw_count });
            }

            // -- Goto mark --
            KeyCode::Char('`') => {
                self.pending = Some(Pending::GotoMark { exact: true });
            }
            KeyCode::Char('\'') => {
                self.pending = Some(Pending::GotoMark { exact: false });
            }

            _ => {}
        }

        Action::Continue
    }

    // ── Visual selection ranges ──────────────────────────────────────────

    /// Save the current visual selection's line range for `'<,'>`.
    fn save_visual_lines(&mut self) {
        if let Some(range) = self.cursor.selection() {
            self.last_visual_lines = Some((range.start.line, range.end.line));
        }
    }

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

        let reg_name = self.selected_register.take();
        self.registers.yank(reg_name, text.clone(), reg_kind);

        self.history.begin(self.cursor.position());
        self.history.record_delete(range.start, &text);
        self.buffer.delete(range);
        self.cursor.clear_anchor();
        self.cursor
            .set_position(range.start, &self.buffer, false);
        self.cursor.clamp(&self.buffer, false);
        self.commit_history();
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
        let reg_name = self.selected_register.take();
        self.registers.yank(reg_name, text, reg_kind);

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

        let reg_name = self.selected_register.take();
        self.registers.yank(reg_name, text.clone(), reg_kind);

        // Delete the selection as one transaction, then begin a new one
        // for the insert phase (so undo restores text, redo re-deletes).
        self.history.begin(self.cursor.position());
        self.history.record_delete(range.start, &text);
        self.buffer.delete(range);
        self.cursor.clear_anchor();
        self.cursor
            .set_position(range.start, &self.buffer, true);
        self.cursor.clamp(&self.buffer, true);
        self.commit_history();

        // Begin a new transaction for the insert session.
        self.history.begin(self.cursor.position());
        self.mode = Mode::Insert;
    }

    /// Indent the visual selection (`>` in visual mode).
    ///
    /// Indents all lines in the selection, then exits visual mode.
    fn visual_indent(&mut self) {
        if !matches!(self.mode, Mode::Visual(_)) {
            return;
        }
        let Some(range) = self.cursor.selection() else {
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        };

        self.cursor.clear_anchor();
        self.mode = Mode::Normal;
        self.indent_lines(range.start.line, range.end.line);
    }

    /// Outdent the visual selection (`<` in visual mode).
    ///
    /// Removes one level of indentation from all lines in the selection,
    /// then exits visual mode.
    fn visual_outdent(&mut self) {
        if !matches!(self.mode, Mode::Visual(_)) {
            return;
        }
        let Some(range) = self.cursor.selection() else {
            self.cursor.clear_anchor();
            self.mode = Mode::Normal;
            return;
        };

        self.cursor.clear_anchor();
        self.mode = Mode::Normal;
        self.outdent_lines(range.start.line, range.end.line);
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
                // Push pre-search position to jump list (search is a jump).
                self.jump_list.push(ss.saved_pos());
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
        let reg_name = self.selected_register.take();
        let reg = self.registers.get(reg_name);
        if reg.is_empty() || count == 0 {
            return;
        }

        let single = reg.content().to_string();
        let text = single.repeat(count);
        let kind = reg.kind();

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
        self.commit_history();
    }

    /// Paste before the cursor (`P` / `3P` in normal mode).
    ///
    /// With count, the register content is pasted `count` times.
    fn paste_before(&mut self, count: usize) {
        let reg_name = self.selected_register.take();
        let reg = self.registers.get(reg_name);
        if reg.is_empty() || count == 0 {
            return;
        }

        let single = reg.content().to_string();
        let text = single.repeat(count);
        let kind = reg.kind();

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
        self.commit_history();
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

    /// Scroll down by half a page (`Ctrl+D` in Vim).
    ///
    /// Moves both the viewport and the cursor down by `count * half_page`
    /// lines. The cursor stays at the same relative position in the viewport.
    fn scroll_half_page_down(&mut self, count: usize) {
        let pe = self.mode.cursor_past_end();
        let half = self.last_text_height.max(2) / 2;
        let distance = half * count;
        let last_line = self.buffer.line_count().saturating_sub(1);

        // Move cursor down.
        self.cursor.move_down(distance, &self.buffer, pe);

        // Move viewport down by the same amount (clamped so we don't scroll
        // past the last line).
        let new_top = (self.view.top_line() + distance).min(last_line);
        self.view.set_top_line(new_top);
    }

    /// Scroll up by half a page (`Ctrl+U` in Vim).
    ///
    /// Moves both the viewport and the cursor up by `count * half_page` lines.
    fn scroll_half_page_up(&mut self, count: usize) {
        let pe = self.mode.cursor_past_end();
        let half = self.last_text_height.max(2) / 2;
        let distance = half * count;

        // Move cursor up.
        self.cursor.move_up(distance, &self.buffer, pe);

        // Move viewport up.
        let new_top = self.view.top_line().saturating_sub(distance);
        self.view.set_top_line(new_top);
    }

    /// Join `count` lines starting from the cursor line (`J` / `3J` in Vim).
    ///
    /// Each join removes the newline at the end of the current line, strips
    /// leading whitespace from the next line, and inserts a single space
    /// (unless the current line ends with a space or the next line is empty).
    /// The cursor is placed at the join point (end of original line content).
    ///
    /// `3J` joins 3 lines into one (performs 2 joins).
    fn join_lines(&mut self, count: usize) {
        // J with count N joins N lines, which means N-1 join operations.
        let joins = if count > 1 { count - 1 } else { 1 };

        let line = self.cursor.line();
        if line + 1 >= self.buffer.line_count() {
            return; // Nothing below to join.
        }

        self.history.begin(self.cursor.position());

        let mut join_col = 0; // Track where the last join happened.

        for _ in 0..joins {
            let cur_line = self.cursor.line();
            if cur_line + 1 >= self.buffer.line_count() {
                break; // No more lines below.
            }

            let cur_content_len = self.buffer.line_content_len(cur_line).unwrap_or(0);

            // Check if current line ends with whitespace (skip adding space).
            let ends_with_space = if cur_content_len > 0 {
                let last_char_pos = Position::new(cur_line, cur_content_len - 1);
                self.buffer
                    .char_at(last_char_pos)
                    .is_some_and(|c| c == ' ' || c == '\t')
            } else {
                false
            };

            // Count leading whitespace on the next line.
            let next_line = cur_line + 1;
            let next_leading = self.buffer.line(next_line).map_or(0, |line_text| {
                line_text
                    .chars()
                    .take_while(|ch| (*ch == ' ' || *ch == '\t') && *ch != '\n')
                    .count()
            });

            let next_content_len = self.buffer.line_content_len(next_line).unwrap_or(0);
            let next_is_empty = next_content_len == 0 || next_content_len == next_leading;

            // Delete from end of current line content through the leading
            // whitespace of the next line (this removes the newline + whitespace).
            let from = Position::new(cur_line, cur_content_len);
            let to = Position::new(next_line, next_leading);
            let range = Range::new(from, to);

            let deleted = self
                .buffer
                .slice(range)
                .map(|s| s.to_string())
                .unwrap_or_default();
            self.history.record_delete(from, &deleted);
            self.buffer.delete(range);

            // Insert a space at the join point (unless current line ends
            // with space, or next line was empty/all-whitespace).
            if !ends_with_space && !next_is_empty && cur_content_len > 0 {
                let insert_pos = Position::new(cur_line, cur_content_len);
                self.history.record_insert(insert_pos, " ");
                self.buffer.insert(insert_pos, " ");
            }
            join_col = cur_content_len;
        }

        // Place cursor at the join point.
        let final_pos = Position::new(self.cursor.line(), join_col);
        self.cursor.set_position(final_pos, &self.buffer, false);
        self.commit_history();
    }

    /// Toggle case of `count` characters at the cursor (`~` / `3~` in Vim).
    ///
    /// Swaps uppercase ↔ lowercase for each character, advancing the cursor.
    /// Does not cross line boundaries.
    fn toggle_case(&mut self, count: usize) {
        let pos = self.cursor.position();
        let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);

        if line_len == 0 || pos.col >= line_len {
            return;
        }

        let end_col = (pos.col + count).min(line_len);
        let range = Range::new(pos, Position::new(pos.line, end_col));

        let old_text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Toggle each character's case.
        let new_text: String = old_text
            .chars()
            .map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().next().unwrap_or(c)
                } else if c.is_lowercase() {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c
                }
            })
            .collect();

        if old_text == new_text && count <= 1 {
            // Nothing changed but still advance cursor (Vim behavior).
            let new_col = (pos.col + 1).min(line_len.saturating_sub(1));
            self.cursor
                .set_position(Position::new(pos.line, new_col), &self.buffer, false);
            return;
        }

        self.history.begin(pos);
        self.history.record_delete(pos, &old_text);
        self.buffer.delete(range);
        self.history.record_insert(pos, &new_text);
        self.buffer.insert(pos, &new_text);

        // Cursor advances past the toggled region (Vim lands on last char
        // if at end of line, otherwise one past).
        let new_col = end_col.min(line_len.saturating_sub(1));
        self.cursor
            .set_position(Position::new(pos.line, new_col), &self.buffer, false);
        self.commit_history();
    }

    /// Replace `count` characters at the cursor with `ch` (`r` / `3ra` in Vim).
    ///
    /// Stays in normal mode. Does not cross line boundaries. If fewer than
    /// `count` characters remain on the line, does nothing (Vim behavior).
    fn replace_chars(&mut self, ch: char, count: usize) {
        let pos = self.cursor.position();
        let line_len = self.buffer.line_content_len(pos.line).unwrap_or(0);

        // Vim: `r` requires exactly `count` characters available.
        if line_len == 0 || pos.col + count > line_len {
            return;
        }

        let end = Position::new(pos.line, pos.col + count);
        let range = Range::new(pos, end);

        let old_text = self
            .buffer
            .slice(range)
            .map(|s| s.to_string())
            .unwrap_or_default();

        let new_text: String = std::iter::repeat_n(ch, count).collect();

        self.history.begin(pos);
        self.history.record_delete(pos, &old_text);
        self.buffer.delete(range);
        self.history.record_insert(pos, &new_text);
        self.buffer.insert(pos, &new_text);
        // Cursor lands on the last replaced character.
        let final_col = pos.col + count - 1;
        self.cursor
            .set_position(Position::new(pos.line, final_col), &self.buffer, false);
        self.commit_history();
    }

    // ── Indent / outdent ────────────────────────────────────────────────

    /// Width of one indentation level (in spaces).
    const INDENT_WIDTH: usize = 4;

    /// Dispatch an operator: routes `>` / `<` to indent/outdent, others to
    /// the standard `apply_operator` path.
    fn execute_operator(&mut self, op: char, range: Range, linewise: bool) -> Action {
        match op {
            '>' | '<' => {
                self.indent_outdent_range(op, range);
                Action::Continue
            }
            _ => self.apply_operator(op, range, linewise),
        }
    }

    /// Indent or outdent lines covered by an arbitrary range.
    ///
    /// All `>` / `<` operations are linewise — even `>w` indents the full
    /// line(s) the motion spans. If the range ends at column 0 of a line,
    /// that line is excluded (it's the exclusive end of a linewise range).
    fn indent_outdent_range(&mut self, op: char, range: Range) {
        // If the range starts at or past the content end of its line (e.g.,
        // an inner-brace text object starting right after the `{` at the end
        // of a line), the first indentable line is the next one.
        let start_content_len = self.buffer.line_content_len(range.start.line).unwrap_or(0);
        let first_line = if start_content_len > 0 && range.start.col >= start_content_len {
            range.start.line + 1
        } else {
            range.start.line
        };

        let last_line = if range.end.col == 0 && range.end.line > first_line {
            range.end.line - 1
        } else {
            range.end.line
        };

        if first_line > last_line {
            return;
        }

        match op {
            '>' => self.indent_lines(first_line, last_line),
            '<' => self.outdent_lines(first_line, last_line),
            _ => {}
        }
    }

    /// Indent or outdent `count` lines starting from the cursor (`>>` / `<<`).
    fn indent_outdent_line_op(&mut self, op: char, count: usize) {
        let first = self.cursor.line();
        let last = (first + count - 1).min(self.buffer.line_count().saturating_sub(1));

        match op {
            '>' => self.indent_lines(first, last),
            '<' => self.outdent_lines(first, last),
            _ => {}
        }
    }

    /// Indent lines `first..=last` by one level (prepend spaces).
    ///
    /// Empty lines are skipped (Vim behavior). The cursor is placed at the
    /// first non-blank of the first affected line.
    fn indent_lines(&mut self, first: usize, last: usize) {
        let indent: String = std::iter::repeat_n(' ', Self::INDENT_WIDTH).collect();

        self.history.begin(self.cursor.position());

        for line in first..=last {
            // Skip empty lines (Vim doesn't indent empty lines).
            if self.buffer.line_content_len(line).unwrap_or(0) == 0 {
                continue;
            }
            let pos = Position::new(line, 0);
            self.history.record_insert(pos, &indent);
            self.buffer.insert(pos, &indent);
        }

        // Cursor goes to first non-blank of first line.
        self.cursor
            .set_position(Position::new(first, 0), &self.buffer, false);
        self.cursor.move_to_first_non_blank(&self.buffer, false);
        self.commit_history();

        let count = last - first + 1;
        if count > 1 {
            self.set_message(format!("{count} lines indented"));
        }
    }

    /// Outdent lines `first..=last` by one level (remove leading whitespace).
    ///
    /// Removes up to `INDENT_WIDTH` leading spaces, or one leading tab.
    /// The cursor is placed at the first non-blank of the first affected line.
    fn outdent_lines(&mut self, first: usize, last: usize) {
        self.history.begin(self.cursor.position());

        for line in first..=last {
            let line_text = self.buffer.line(line).map(|s| s.to_string());
            let Some(text) = line_text else { continue };

            // Count leading whitespace to remove (up to one indent level).
            let mut remove = 0;
            for ch in text.chars() {
                if ch == '\t' && remove == 0 {
                    remove = 1;
                    break;
                } else if ch == ' ' && remove < Self::INDENT_WIDTH {
                    remove += 1;
                } else {
                    break;
                }
            }

            if remove > 0 {
                let from = Position::new(line, 0);
                let to = Position::new(line, remove);
                let range = Range::new(from, to);
                let deleted = self
                    .buffer
                    .slice(range)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                self.history.record_delete(from, &deleted);
                self.buffer.delete(range);
            }
        }

        // Cursor goes to first non-blank of first line.
        self.cursor
            .set_position(Position::new(first, 0), &self.buffer, false);
        self.cursor.move_to_first_non_blank(&self.buffer, false);
        self.commit_history();

        let count = last - first + 1;
        if count > 1 {
            self.set_message(format!("{count} lines outdented"));
        }
    }

    // ── Scroll positioning ─────────────────────────────────────────────

    /// Scroll so the cursor line is at the center of the viewport (`zz`).
    const fn scroll_cursor_center(&mut self) {
        let half = self.last_text_height / 2;
        let new_top = self.cursor.line().saturating_sub(half);
        self.view.set_top_line(new_top);
    }

    /// Scroll so the cursor line is at the top of the viewport (`zt`).
    const fn scroll_cursor_top(&mut self) {
        self.view.set_top_line(self.cursor.line());
    }

    /// Scroll so the cursor line is at the bottom of the viewport (`zb`).
    const fn scroll_cursor_bottom(&mut self) {
        let new_top = self.cursor.line().saturating_sub(self.last_text_height.saturating_sub(1));
        self.view.set_top_line(new_top);
    }

    // ── Marks ──────────────────────────────────────────────────────────

    /// Jump to a mark position.
    ///
    /// If `exact` is true (`` ` `` prefix), jump to the exact position.
    /// If `exact` is false (`'` prefix), jump to the first non-blank of
    /// the mark's line.
    fn goto_mark(&mut self, ch: char, exact: bool) {
        let idx = (ch as u8 - b'a') as usize;
        if let Some(pos) = self.marks[idx] {
            let pe = self.mode.cursor_past_end();
            if exact {
                self.cursor.set_position(pos, &self.buffer, pe);
            } else {
                self.cursor
                    .set_position(Position::new(pos.line, 0), &self.buffer, pe);
                self.cursor.move_to_first_non_blank(&self.buffer, pe);
            }
        } else {
            self.set_error(format!("E20: Mark not set: {ch}"));
        }
    }

    /// Compute the operator range for a mark motion.
    ///
    /// `'a` produces a linewise range, `` `a `` produces a charwise range.
    fn mark_operator_range(
        &self,
        ch: char,
        exact: bool,
        _op_count: usize,
    ) -> Option<Range> {
        let idx = (ch as u8 - b'a') as usize;
        let mark_pos = self.marks[idx]?;
        let start = self.cursor.position();

        if exact {
            // `` `a `` — charwise (inclusive).
            let (from, to) = if start <= mark_pos {
                (start, mark_pos)
            } else {
                (mark_pos, start)
            };
            // Extend to include the character at the far end.
            let end_line_len = self.buffer.line_content_len(to.line).unwrap_or(0);
            let extended = if to.col < end_line_len {
                Position::new(to.line, to.col + 1)
            } else {
                Position::new(to.line, end_line_len)
            };
            Some(Range::new(from, extended))
        } else {
            // `'a` — linewise.
            self.linewise_range(start, mark_pos)
        }
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

        let reg_name = self.selected_register.take();
        self.registers.yank(reg_name, text.clone(), RegisterKind::Char);
        self.history.begin(pos);
        self.history.record_delete(pos, &text);
        self.buffer.delete(range);
        self.cursor.clamp(&self.buffer, pe);
        self.commit_history();
    }

}

// ─── Bracket matching ───────────────────────────────────────────────────────

/// Find the matching bracket for the character at `pos`.
///
/// Supports `()`, `[]`, `{}`. Handles nesting by tracking depth. Scans
/// forward for open brackets, backward for close brackets. Returns `None`
/// if the character at `pos` is not a bracket or no match is found.
/// Translate a Vim-style replacement string to `regex` crate syntax.
///
/// Vim uses `&` for the whole match and `\1`-`\9` for capture groups.
/// The `regex` crate uses `$0` and `$1`-`$9`. We also handle:
///
/// - `\&` → literal `&`
/// - `\\` → literal `\`
/// - `\n` → newline
fn translate_replacement(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some(&next @ '1'..='9') => {
                    // `\1`-`\9` → `$1`-`$9` (capture group).
                    result.push('$');
                    result.push(next);
                    chars.next();
                }
                Some(&'0') => {
                    // `\0` → `$0` (whole match).
                    result.push('$');
                    result.push('0');
                    chars.next();
                }
                Some(&'&') => {
                    // `\&` → literal `&`.
                    result.push('&');
                    chars.next();
                }
                Some(&'\\') => {
                    // `\\` → literal `\`.
                    result.push('\\');
                    chars.next();
                }
                Some(&'n') => {
                    // `\n` → newline.
                    result.push('\n');
                    chars.next();
                }
                _ => {
                    // Pass through other `\X` sequences.
                    result.push('\\');
                }
            }
        } else if ch == '&' {
            // `&` → `$0` (whole match).
            result.push_str("$0");
        } else if ch == '$' {
            // Escape literal `$` to prevent the regex crate from
            // interpreting it as a capture group reference.
            result.push_str("$$");
        } else {
            result.push(ch);
        }
    }

    result
}

fn find_matching_bracket(buf: &Buffer, pos: Position) -> Option<Position> {
    let ch = buf.char_at(pos)?;

    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };

    let rope = buf.rope();
    let total = rope.len_chars();
    let start_idx = rope.line_to_char(pos.line) + pos.col;

    let mut depth: i32 = 0;

    if forward {
        for i in start_idx..total {
            let c = rope.char(i);
            if c == open {
                depth += 1;
            }
            if c == close {
                depth -= 1;
            }
            if depth == 0 {
                return buf.char_idx_to_pos(i);
            }
        }
    } else {
        for i in (0..=start_idx).rev() {
            let c = rope.char(i);
            if c == close {
                depth += 1;
            }
            if c == open {
                depth -= 1;
            }
            if depth == 0 {
                return buf.char_idx_to_pos(i);
            }
        }
    }

    None
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

        // Macro recording: `q` in normal mode stops recording. All other
        // keys are pushed to the macro register (unless we're replaying).
        if let Some(idx) = self.macro_recording {
            let is_stop_key = matches!(self.mode, Mode::Normal)
                && key.code == KeyCode::Char('q')
                && !key.modifiers.contains(Modifiers::CTRL);
            if is_stop_key {
                self.macro_recording = None;
                return Action::Continue;
            }
            // Record every key during macro recording (skip during replay
            // of either macros or dot-repeat, to avoid double-recording).
            if !self.macro_replaying && !self.dot_replaying {
                self.macro_keys[idx].push(*key);
            }
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
        self.last_frame_size = (w, h);

        if h < 2 {
            // Too small for multi-window — just render the active window.
            let selection = match self.mode {
                Mode::Visual(kind) => self.cursor.selection().map(|r| (r, kind)),
                _ => None,
            };
            let buf_info = self.buf_info_label();
            self.cursor_screen = self.view.render(
                &self.buffer, &self.cursor, self.mode, selection, &buf_info,
                frame, 0, 0, w, h,
            );
            return;
        }

        // Reserve the bottom row for command/message line.
        let main_height = h - 1;
        let main_area = Rect { x: 0, y: 0, w, h: main_height };

        // Compute layout rectangles for all windows.
        let rects = self.split.layout(main_area);
        let buf_info = self.buf_info_label();

        // Render each window into its rectangle.
        for &(win_id, rect) in &rects {
            if win_id == self.active_win_id {
                // Active window: use flat fields.
                let selection = match self.mode {
                    Mode::Visual(kind) => self.cursor.selection().map(|r| (r, kind)),
                    _ => None,
                };
                // Store text height for active window (for Ctrl+D/U).
                self.last_text_height = rect.h.saturating_sub(1) as usize;
                self.cursor_screen = self.view.render(
                    &self.buffer, &self.cursor, self.mode, selection, &buf_info,
                    frame, rect.x, rect.y, rect.w, rect.h,
                );
                // Highlight search matches in the active window.
                let hl_pattern = if self.search.is_some() {
                    self.search.as_ref().map_or("", |ss| ss.input())
                } else {
                    &self.last_search
                };
                if !hl_pattern.is_empty() {
                    view::highlight_matches(
                        &self.view, frame, &self.buffer, hl_pattern,
                        rect.x, rect.y, rect.w, rect.h,
                    );
                }
            } else {
                // Inactive window: render with its own cursor/view.
                self.render_inactive_window(win_id, &buf_info, frame, rect);
            }
        }

        // Draw vertical separators.
        let separators = self.split.separators(main_area);
        for (sx, sy, sh) in separators {
            for row in 0..sh {
                frame.set(
                    sx,
                    sy + row,
                    n_term::cell::Cell::styled(
                        '│',
                        n_term::color::CellColor::Default,
                        n_term::color::CellColor::Default,
                        n_term::cell::Attr::DIM,
                        n_term::cell::UnderlineStyle::None,
                    ),
                );
            }
        }

        // Bottom row: command line, search prompt, or message.
        let bottom_y = h - 1;

        if let Some(ref ss) = self.search {
            let search_cursor = view::render_search_line(
                frame, ss.prefix(), ss.input(), ss.input_cursor(),
                0, bottom_y, w,
            );
            self.cursor_screen = search_cursor;
        } else if self.mode == Mode::Command {
            let cmd_cursor = view::render_command_line(
                frame, self.cmdline.input(), self.cmdline.cursor(),
                0, bottom_y, w,
            );
            self.cursor_screen = cmd_cursor;
        } else if let Some(idx) = self.macro_recording {
            #[allow(clippy::cast_possible_truncation)]
            let ch = (b'a' + idx as u8) as char;
            let msg = format!("recording @{ch}");
            view::render_message_line(frame, &msg, false, 0, bottom_y, w);
        } else if let Some(ref msg) = self.message {
            view::render_message_line(frame, msg, self.message_is_error, 0, bottom_y, w);
        } else {
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

    /// Create a Ctrl+key press event.
    fn ctrl(ch: char) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: Modifiers::CTRL,
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

        // Move to first line, . repeats
        feed(&mut e, &[press('g'), press('g'), press('.')]);
        // gg goes to first line. Cursor at 'X' (col 0).
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

    // ── r (replace char) ──────────────────────────────────────────────────

    #[test]
    fn r_replace_single_char() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('r'), press('X')]);
        assert_eq!(e.buffer.contents(), "Xello");
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn r_replace_middle_of_line() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('l'), press('l'), press('r'), press('X')]);
        assert_eq!(e.buffer.contents(), "heXlo");
        assert_eq!(e.cursor.col(), 2);
    }

    #[test]
    fn r_with_count() {
        let mut e = editor_with("abcdef");
        feed(&mut e, &[press('3'), press('r'), press('X')]);
        assert_eq!(e.buffer.contents(), "XXXdef");
        // Cursor on last replaced char.
        assert_eq!(e.cursor.col(), 2);
    }

    #[test]
    fn r_count_exceeds_line_does_nothing() {
        let mut e = editor_with("ab");
        feed(&mut e, &[press('5'), press('r'), press('X')]);
        // Count 5 > line length 2: no replacement (Vim behavior).
        assert_eq!(e.buffer.contents(), "ab");
    }

    #[test]
    fn r_on_empty_line_does_nothing() {
        let mut e = editor_with("");
        feed(&mut e, &[press('r'), press('X')]);
        assert_eq!(e.buffer.contents(), "");
    }

    #[test]
    fn r_escape_cancels() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('r'), esc()]);
        assert_eq!(e.buffer.contents(), "hello");
        assert!(e.pending.is_none());
    }

    #[test]
    fn r_undo_restores() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('r'), press('X'), press('u')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn r_dot_repeat() {
        let mut e = editor_with("abcdef");
        // ra replaces 'a' with 'a'... wait, let's use rX, move right, .
        feed(
            &mut e,
            &[press('r'), press('X'), press('l'), press('.')],
        );
        // rX: "Xbcdef" cursor at 0. l: cursor at 1 ('b'). .: rX on 'b' → "XXcdef"
        assert_eq!(e.buffer.contents(), "XXcdef");
    }

    #[test]
    fn r_dot_repeat_with_count() {
        let mut e = editor_with("abcdefgh");
        // 3rX replaces 'abc' → "XXXdefgh", cursor at 2.
        // l → cursor at 3 ('d'). . → repeats 3rX → "XXXXXXgh"
        feed(
            &mut e,
            &[press('3'), press('r'), press('X'), press('l'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "XXXXXXgh");
    }

    #[test]
    fn r_dot_repeat_count_override() {
        let mut e = editor_with("abcdefgh");
        // rX → "Xbcdefgh" at 0, l to 1. 3. → replaces 3 chars.
        feed(
            &mut e,
            &[
                press('r'),
                press('X'),
                press('l'),
                press('3'),
                press('.'),
            ],
        );
        assert_eq!(e.buffer.contents(), "XXXXefgh");
    }

    // ── J (join lines) ─────────────────────────────────────────────────────

    #[test]
    fn j_join_basic() {
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('J')]);
        assert_eq!(e.buffer.contents(), "hello world");
        // Cursor at the join point (the space).
        assert_eq!(e.cursor.col(), 5);
    }

    #[test]
    fn j_join_strips_leading_whitespace() {
        let mut e = editor_with("hello\n    world");
        feed(&mut e, &[press('J')]);
        assert_eq!(e.buffer.contents(), "hello world");
    }

    #[test]
    fn j_join_with_count() {
        let mut e = editor_with("one\ntwo\nthree");
        // 3J joins 3 lines (2 join operations).
        feed(&mut e, &[press('3'), press('J')]);
        assert_eq!(e.buffer.contents(), "one two three");
    }

    #[test]
    fn j_join_empty_next_line() {
        let mut e = editor_with("hello\n\nworld");
        // Joining with an empty line: no space inserted.
        feed(&mut e, &[press('J')]);
        assert_eq!(e.buffer.contents(), "hello\nworld");
    }

    #[test]
    fn j_join_on_last_line_does_nothing() {
        let mut e = editor_with("only line");
        feed(&mut e, &[press('J')]);
        assert_eq!(e.buffer.contents(), "only line");
    }

    #[test]
    fn j_join_cursor_already_ends_with_space() {
        let mut e = editor_with("hello \nworld");
        feed(&mut e, &[press('J')]);
        // Line already ends with space — don't add another.
        assert_eq!(e.buffer.contents(), "hello world");
    }

    #[test]
    fn j_join_undo() {
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('J'), press('u')]);
        assert_eq!(e.buffer.contents(), "hello\nworld");
    }

    #[test]
    fn j_dot_repeat() {
        let mut e = editor_with("a\nb\nc\nd");
        // J joins "a\nb" → "a b", . joins "a b\nc" → "a b c"
        feed(&mut e, &[press('J'), press('.')]);
        assert_eq!(e.buffer.contents(), "a b c\nd");
    }

    #[test]
    fn j_join_empty_current_line() {
        let mut e = editor_with("\nhello");
        // Current line is empty, next line has content.
        // No space should be inserted (current line has 0 content).
        feed(&mut e, &[press('J')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    // ── ~ (toggle case) ────────────────────────────────────────────────────

    #[test]
    fn tilde_lowercase_to_upper() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('~')]);
        assert_eq!(e.buffer.contents(), "Hello");
        // Cursor advances to next char.
        assert_eq!(e.cursor.col(), 1);
    }

    #[test]
    fn tilde_uppercase_to_lower() {
        let mut e = editor_with("HELLO");
        feed(&mut e, &[press('~')]);
        assert_eq!(e.buffer.contents(), "hELLO");
        assert_eq!(e.cursor.col(), 1);
    }

    #[test]
    fn tilde_with_count() {
        let mut e = editor_with("heLLo");
        feed(&mut e, &[press('5'), press('~')]);
        assert_eq!(e.buffer.contents(), "HEllO");
        // Cursor on last char (col 4, line has 5 chars).
        assert_eq!(e.cursor.col(), 4);
    }

    #[test]
    fn tilde_non_alpha_advances() {
        let mut e = editor_with("123ab");
        feed(&mut e, &[press('~')]);
        // '1' has no case — cursor still advances.
        assert_eq!(e.buffer.contents(), "123ab");
        assert_eq!(e.cursor.col(), 1);
    }

    #[test]
    fn tilde_at_end_of_line() {
        let mut e = editor_with("aBc");
        // Move to last char, toggle.
        feed(&mut e, &[press('$'), press('~')]);
        assert_eq!(e.buffer.contents(), "aBC");
        // Cursor stays on last char (can't advance further).
        assert_eq!(e.cursor.col(), 2);
    }

    #[test]
    fn tilde_on_empty_line() {
        let mut e = editor_with("");
        feed(&mut e, &[press('~')]);
        assert_eq!(e.buffer.contents(), "");
    }

    #[test]
    fn tilde_undo() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('3'), press('~'), press('u')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn tilde_dot_repeat() {
        let mut e = editor_with("abcdef");
        // ~ toggles 'a' → 'A', cursor at 1. . toggles 'b' → 'B', cursor at 2.
        feed(&mut e, &[press('~'), press('.')]);
        assert_eq!(e.buffer.contents(), "ABcdef");
    }

    #[test]
    fn tilde_dot_repeat_with_count() {
        let mut e = editor_with("abcdefgh");
        // 3~ toggles "abc" → "ABC", cursor at 3. . repeats 3~ on "def" → "DEF"
        feed(&mut e, &[press('3'), press('~'), press('.')]);
        assert_eq!(e.buffer.contents(), "ABCDEFgh");
    }

    #[test]
    fn tilde_count_clamps_to_line_end() {
        let mut e = editor_with("ab");
        // 10~ on a 2-char line: toggles both, cursor on last char.
        feed(&mut e, &[press('1'), press('0'), press('~')]);
        assert_eq!(e.buffer.contents(), "AB");
        assert_eq!(e.cursor.col(), 1);
    }

    // ── Ctrl+D / Ctrl+U (half-page scroll) ──────────────────────────────

    #[test]
    fn ctrl_d_moves_cursor_down() {
        let mut e = editor_with(
            &(0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        );
        // Default last_text_height is 24, half = 12.
        feed(&mut e, &[ctrl('d')]);
        assert_eq!(e.cursor.line(), 12);
    }

    #[test]
    fn ctrl_u_moves_cursor_up() {
        let mut e = editor_with(
            &(0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        );
        // Move to line 30 first.
        feed(&mut e, &[press('3'), press('0'), press('j')]);
        assert_eq!(e.cursor.line(), 30);
        feed(&mut e, &[ctrl('u')]);
        assert_eq!(e.cursor.line(), 18); // 30 - 12 = 18
    }

    #[test]
    fn ctrl_d_clamps_at_end() {
        let mut e = editor_with("a\nb\nc\nd\ne");
        feed(&mut e, &[ctrl('d')]);
        // Only 5 lines. Cursor clamped to last line.
        assert_eq!(e.cursor.line(), 4);
    }

    #[test]
    fn ctrl_u_clamps_at_top() {
        let mut e = editor_with(
            &(0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        );
        // Already at top — stays at line 0.
        feed(&mut e, &[ctrl('u')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_d_with_count() {
        let mut e = editor_with(
            &(0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        );
        // 3 Ctrl+D = 3 * 12 = 36 lines down.
        feed(&mut e, &[press('3'), ctrl('d')]);
        assert_eq!(e.cursor.line(), 36);
    }

    #[test]
    fn ctrl_d_in_visual_mode() {
        let mut e = editor_with(
            &(0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        );
        // Enter visual, Ctrl+D — selection extends.
        feed(&mut e, &[press('v'), ctrl('d')]);
        assert_eq!(e.cursor.line(), 12);
        assert!(e.cursor.has_selection());
        assert_eq!(e.cursor.anchor().unwrap().line, 0);
    }

    // ── Indent (>>) ─────────────────────────────────────────────────────

    #[test]
    fn indent_single_line() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('>'), press('>')]);
        assert_eq!(e.buffer.contents(), "    hello");
        // Cursor at first non-blank (col 4).
        assert_eq!(e.cursor.col(), 4);
    }

    #[test]
    fn indent_with_count() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // 3>> = indent 3 lines.
        feed(&mut e, &[press('3'), press('>'), press('>')]);
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\n    ccc\nddd");
    }

    #[test]
    fn indent_skips_empty_lines() {
        let mut e = editor_with("aaa\n\nccc");
        feed(&mut e, &[press('3'), press('>'), press('>')]);
        // Empty line stays empty.
        assert_eq!(e.buffer.contents(), "    aaa\n\n    ccc");
    }

    #[test]
    fn indent_stacks() {
        let mut e = editor_with("hello");
        // >> twice = 8 spaces.
        feed(&mut e, &[press('>'), press('>'), press('>'), press('>')]);
        assert_eq!(e.buffer.contents(), "        hello");
    }

    #[test]
    fn indent_with_motion_j() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // >j = indent current line and the one below.
        feed(&mut e, &[press('>'), press('j')]);
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\nccc");
    }

    #[test]
    fn indent_with_motion_to_last_line() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // >G = indent from cursor line to end of file.
        feed(&mut e, &[press('>'), press('G')]);
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\n    ccc");
    }

    #[test]
    fn indent_undo() {
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('>'), press('>'), press('u')]);
        assert_eq!(e.buffer.contents(), "hello\nworld");
    }

    #[test]
    fn indent_dot_repeat() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // >> on first line, j to next, . to repeat.
        feed(
            &mut e,
            &[press('>'), press('>'), press('j'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\nccc");
    }

    #[test]
    fn indent_dot_repeat_with_count() {
        let mut e = editor_with("a\nb\nc\nd\ne");
        // 2>> indents 2 lines (a, b). Cursor on line 0.
        // j j moves to line 2. 3. overrides count → 3>> → indents 3 lines (c, d, e).
        feed(
            &mut e,
            &[
                press('2'), press('>'), press('>'),
                press('j'), press('j'),
                press('3'), press('.'),
            ],
        );
        assert_eq!(e.buffer.contents(), "    a\n    b\n    c\n    d\n    e");
    }

    // ── Outdent (<<) ────────────────────────────────────────────────────

    #[test]
    fn outdent_single_line() {
        let mut e = editor_with("    hello");
        feed(&mut e, &[press('<'), press('<')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn outdent_partial_spaces() {
        // Only 2 spaces — remove what's there.
        let mut e = editor_with("  hello");
        feed(&mut e, &[press('<'), press('<')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn outdent_tab() {
        let mut e = editor_with("\thello");
        feed(&mut e, &[press('<'), press('<')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn outdent_no_leading_whitespace() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('<'), press('<')]);
        // Nothing to remove — stays the same.
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn outdent_with_count() {
        let mut e = editor_with("    aaa\n    bbb\n    ccc\nddd");
        // 3<< = outdent 3 lines.
        feed(&mut e, &[press('3'), press('<'), press('<')]);
        assert_eq!(e.buffer.contents(), "aaa\nbbb\nccc\nddd");
    }

    #[test]
    fn outdent_with_motion_j() {
        let mut e = editor_with("    aaa\n    bbb\nccc");
        feed(&mut e, &[press('<'), press('j')]);
        assert_eq!(e.buffer.contents(), "aaa\nbbb\nccc");
    }

    #[test]
    fn outdent_undo() {
        let mut e = editor_with("    hello");
        feed(&mut e, &[press('<'), press('<'), press('u')]);
        assert_eq!(e.buffer.contents(), "    hello");
    }

    #[test]
    fn outdent_dot_repeat() {
        let mut e = editor_with("    aaa\n    bbb\nccc");
        feed(
            &mut e,
            &[press('<'), press('<'), press('j'), press('.')],
        );
        assert_eq!(e.buffer.contents(), "aaa\nbbb\nccc");
    }

    #[test]
    fn outdent_more_than_indent_width() {
        // 8 spaces: << removes 4, leaving 4.
        let mut e = editor_with("        hello");
        feed(&mut e, &[press('<'), press('<')]);
        assert_eq!(e.buffer.contents(), "    hello");
    }

    // ── Visual indent / outdent ─────────────────────────────────────────

    #[test]
    fn visual_indent_char_mode() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // v j > = select 2 lines, indent.
        feed(&mut e, &[press('v'), press('j'), press('>')]);
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\nccc");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn visual_indent_line_mode() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // V j > = select 2 lines, indent.
        feed(&mut e, &[press('V'), press('j'), press('>')]);
        assert_eq!(e.buffer.contents(), "    aaa\n    bbb\nccc");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn visual_outdent() {
        let mut e = editor_with("    aaa\n    bbb\nccc");
        feed(&mut e, &[press('V'), press('j'), press('<')]);
        assert_eq!(e.buffer.contents(), "aaa\nbbb\nccc");
        assert_eq!(e.mode, Mode::Normal);
    }

    // ── Indent with text objects ────────────────────────────────────────

    #[test]
    fn indent_inner_curly_braces() {
        let mut e = editor_with("fn main() {\n    x\n    y\n}");
        // Move cursor inside braces, >iB to indent inner block.
        feed(
            &mut e,
            &[press('j'), press('>'), press('i'), press('B')],
        );
        assert_eq!(
            e.buffer.contents(),
            "fn main() {\n        x\n        y\n}"
        );
    }

    // ── D (d$) — delete to end of line ──────────────────────────────────

    #[test]
    fn d_upper_basic() {
        let mut e = editor_with("hello world");
        // Move to 'w' (col 6), D deletes "world".
        feed(&mut e, &[press('f'), press('w'), press('D')]);
        assert_eq!(e.buffer.contents(), "hello ");
    }

    #[test]
    fn d_upper_at_end_of_line() {
        let mut e = editor_with("hello");
        // Move to end, D does nothing.
        feed(&mut e, &[press('$'), press('D')]);
        assert_eq!(e.buffer.contents(), "hell");
    }

    #[test]
    fn d_upper_dot_repeat() {
        let mut e = editor_with("aaa bbb\nccc ddd");
        // fw, D, j0fw, .
        feed(
            &mut e,
            &[
                press('f'), press('b'), press('D'),
                press('j'), press('0'),
                press('f'), press('d'), press('.'),
            ],
        );
        assert_eq!(e.buffer.contents(), "aaa \nccc ");
    }

    #[test]
    fn d_upper_stores_in_register() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('f'), press('w'), press('D')]);
        assert_eq!(e.registers.get(None).content(), "world");
    }

    // ── C (c$) — change to end of line ──────────────────────────────────

    #[test]
    fn c_upper_basic() {
        let mut e = editor_with("hello world");
        // fw, C, type "xyz", Esc.
        feed(
            &mut e,
            &[
                press('f'), press('w'), press('C'),
                press('x'), press('y'), press('z'), esc(),
            ],
        );
        assert_eq!(e.buffer.contents(), "hello xyz");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn c_upper_enters_insert() {
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('C')]);
        assert_eq!(e.mode, Mode::Insert);
    }

    #[test]
    fn c_upper_dot_repeat() {
        let mut e = editor_with("aaa bbb\nccc ddd");
        // Move to space, C to change, type "!", Esc.
        // Then j0 to next line, move to space, dot repeat.
        feed(
            &mut e,
            &[
                press('f'), press(' '), press('C'),
                press('!'), esc(),
                press('j'), press('0'),
                press('f'), press(' '), press('.'),
            ],
        );
        assert_eq!(e.buffer.contents(), "aaa!\nccc!");
    }

    // ── S (cc) — substitute line ────────────────────────────────────────

    #[test]
    fn s_upper_basic() {
        let mut e = editor_with("hello world");
        // S deletes line content, enters insert.
        feed(
            &mut e,
            &[press('S'), press('h'), press('i'), esc()],
        );
        assert_eq!(e.buffer.contents(), "hi");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn s_upper_with_count() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // 2S = substitute 2 lines. Our cc/S deletes the lines including
        // newlines (same linewise range as dd), then enters insert.
        feed(
            &mut e,
            &[press('2'), press('S'), press('x'), esc()],
        );
        // "aaa\nbbb\n" deleted → "ccc\nddd", then 'x' typed at start.
        assert_eq!(e.buffer.contents(), "xccc\nddd");
    }

    #[test]
    fn s_upper_dot_repeat() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // S on first line: deletes "aaa\n", types 'x', Esc.
        // j to next line, . replays.
        feed(
            &mut e,
            &[
                press('S'), press('x'), esc(),
                press('j'), press('.'),
            ],
        );
        // First S: "aaa\n" deleted → "bbb\nccc", "x" typed → "xbbb\nccc".
        // j to line 1 ("ccc"). Dot replays S on last line (joins with prev).
        assert_eq!(e.buffer.contents(), "xbbbx");
    }

    // ── Indent message ──────────────────────────────────────────────────

    #[test]
    fn indent_multiline_shows_message() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('3'), press('>'), press('>')]);
        assert_eq!(e.message.as_deref(), Some("3 lines indented"));
    }

    #[test]
    fn indent_single_line_no_message() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('>'), press('>')]);
        // Single line indent — no message.
        assert!(e.message.is_none());
    }

    #[test]
    fn outdent_multiline_shows_message() {
        let mut e = editor_with("    aaa\n    bbb\n    ccc");
        feed(&mut e, &[press('3'), press('<'), press('<')]);
        assert_eq!(e.message.as_deref(), Some("3 lines outdented"));
    }

    // ── % (matching bracket) ────────────────────────────────────────────

    #[test]
    fn percent_forward_paren() {
        let mut e = editor_with("(hello)");
        feed(&mut e, &[press('%')]);
        assert_eq!(e.cursor.col(), 6); // on ')'
    }

    #[test]
    fn percent_backward_paren() {
        let mut e = editor_with("(hello)");
        feed(&mut e, &[press('$'), press('%')]);
        assert_eq!(e.cursor.col(), 0); // on '('
    }

    #[test]
    fn percent_square_brackets() {
        let mut e = editor_with("[a, b]");
        feed(&mut e, &[press('%')]);
        assert_eq!(e.cursor.col(), 5);
    }

    #[test]
    fn percent_curly_braces() {
        let mut e = editor_with("{x}");
        feed(&mut e, &[press('%')]);
        assert_eq!(e.cursor.col(), 2);
    }

    #[test]
    fn percent_nested() {
        let mut e = editor_with("((inner))");
        feed(&mut e, &[press('%')]);
        assert_eq!(e.cursor.col(), 8); // outer ) at col 8
    }

    #[test]
    fn percent_multiline() {
        let mut e = editor_with("fn main() {\n    x\n}");
        // Move to '{' on line 0 col 11.
        feed(&mut e, &[press('$'), press('%')]);
        // Should jump to '}' on line 2.
        assert_eq!(e.cursor.line(), 2);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn percent_no_bracket_no_move() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('%')]);
        // Not on a bracket — cursor stays.
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn percent_unmatched_no_move() {
        let mut e = editor_with("(hello");
        feed(&mut e, &[press('%')]);
        // No matching ')' — cursor stays.
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn d_percent_delete_to_matching() {
        let mut e = editor_with("(abc)def");
        feed(&mut e, &[press('d'), press('%')]);
        assert_eq!(e.buffer.contents(), "def");
    }

    #[test]
    fn d_percent_backward() {
        let mut e = editor_with("(abc)def");
        // Move to ')' at col 4, d% backward.
        for _ in 0..4 {
            e.on_event(&press('l'));
        }
        feed(&mut e, &[press('d'), press('%')]);
        assert_eq!(e.buffer.contents(), "def");
    }

    #[test]
    fn v_percent_extends_selection() {
        let mut e = editor_with("(abc)");
        feed(&mut e, &[press('v'), press('%')]);
        assert_eq!(e.cursor.col(), 4); // selection extends to ')'
        assert!(e.cursor.has_selection());
    }

    // ── { / } (paragraph motions) ───────────────────────────────────────

    #[test]
    fn close_brace_next_blank_line() {
        let mut e = editor_with("aaa\nbbb\n\nccc");
        feed(&mut e, &[press('}')]);
        assert_eq!(e.cursor.line(), 2);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn open_brace_prev_blank_line() {
        let mut e = editor_with("aaa\n\nbbb\nccc");
        // Start on last line.
        feed(&mut e, &[press('G'), press('{')]);
        assert_eq!(e.cursor.line(), 1);
    }

    #[test]
    fn close_brace_from_blank_line() {
        let mut e = editor_with("aaa\n\nbbb\n\nccc");
        // Move to blank line 1, then }.
        feed(&mut e, &[press('j'), press('}')]);
        assert_eq!(e.cursor.line(), 3); // next blank line
    }

    #[test]
    fn open_brace_from_blank_line() {
        let mut e = editor_with("aaa\n\nbbb\n\nccc");
        // Move to blank line 3.
        feed(&mut e, &[press('3'), press('j'), press('{')]);
        assert_eq!(e.cursor.line(), 1); // previous blank line
    }

    #[test]
    fn close_brace_no_blank_goes_to_end() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('}')]);
        assert_eq!(e.cursor.line(), 2); // last line
    }

    #[test]
    fn open_brace_no_blank_goes_to_start() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('G'), press('{')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn close_brace_with_count() {
        let mut e = editor_with("a\n\nb\n\nc");
        feed(&mut e, &[press('2'), press('}')]);
        assert_eq!(e.cursor.line(), 3); // second blank line
    }

    #[test]
    fn open_brace_with_count() {
        let mut e = editor_with("a\n\nb\n\nc");
        feed(&mut e, &[press('G'), press('2'), press('{')]);
        assert_eq!(e.cursor.line(), 1); // second blank line back
    }

    #[test]
    fn d_close_brace_linewise() {
        let mut e = editor_with("aaa\nbbb\n\nccc");
        feed(&mut e, &[press('d'), press('}')]);
        // d} from line 0 deletes through line 2 (the blank line).
        assert_eq!(e.buffer.contents(), "ccc");
    }

    #[test]
    fn v_close_brace_selection() {
        let mut e = editor_with("aaa\nbbb\n\nccc");
        feed(&mut e, &[press('v'), press('}')]);
        // Visual selection extends to the blank line.
        assert_eq!(e.cursor.line(), 2);
    }

    // ── zz / zt / zb (scroll positioning) ───────────────────────────────

    #[test]
    fn zz_centers_cursor() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no");
        e.last_text_height = 10;
        e.view.set_top_line(0);
        // Move cursor to line 7.
        feed(&mut e, &[press('7'), press('j')]);
        feed(&mut e, &[press('z'), press('z')]);
        // Center: top_line = 7 - 5 = 2.
        assert_eq!(e.view.top_line(), 2);
    }

    #[test]
    fn zt_puts_cursor_at_top() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj");
        e.last_text_height = 5;
        e.view.set_top_line(0);
        feed(&mut e, &[press('4'), press('j')]);
        feed(&mut e, &[press('z'), press('t')]);
        assert_eq!(e.view.top_line(), 4);
    }

    #[test]
    fn zb_puts_cursor_at_bottom() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj");
        e.last_text_height = 5;
        feed(&mut e, &[press('7'), press('j')]);
        feed(&mut e, &[press('z'), press('b')]);
        // Bottom: top_line = 7 - 4 = 3.
        assert_eq!(e.view.top_line(), 3);
    }

    #[test]
    fn z_escape_cancels() {
        let mut e = editor_with("hello");
        e.view.set_top_line(0);
        feed(&mut e, &[press('z'), esc()]);
        // Nothing changed.
        assert_eq!(e.view.top_line(), 0);
    }

    #[test]
    fn z_enter_same_as_zt() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj");
        e.last_text_height = 5;
        e.view.set_top_line(0);
        feed(&mut e, &[press('4'), press('j')]);
        feed(&mut e, &[press('z'), enter()]);
        assert_eq!(e.view.top_line(), 4);
    }

    #[test]
    fn zz_with_cursor_near_top() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf");
        e.last_text_height = 6;
        e.view.set_top_line(0);
        // Cursor on line 1 — center would want top_line = -2 → clamps to 0.
        feed(&mut e, &[press('j'), press('z'), press('z')]);
        assert_eq!(e.view.top_line(), 0);
    }

    // ── Marks (m / ` / ') ──────────────────────────────────────────────

    #[test]
    fn set_and_goto_mark_exact() {
        let mut e = editor_with("hello\nworld\nfoo");
        // Move to line 1, col 3, set mark 'a'.
        feed(&mut e, &[press('j'), press('l'), press('l'), press('l')]);
        feed(&mut e, &[press('m'), press('a')]);
        // Move elsewhere.
        feed(&mut e, &[press('g'), press('g')]);
        assert_eq!(e.cursor.line(), 0);
        // `a jumps back to exact position.
        feed(&mut e, &[press('`'), press('a')]);
        assert_eq!(e.cursor.line(), 1);
        assert_eq!(e.cursor.col(), 3);
    }

    #[test]
    fn set_and_goto_mark_line() {
        let mut e = editor_with("  hello\n  world\n  foo");
        // Move to line 1, col 4, set mark 'a'.
        feed(
            &mut e,
            &[press('j'), press('l'), press('l'), press('l'), press('l')],
        );
        feed(&mut e, &[press('m'), press('a')]);
        // Move elsewhere.
        feed(&mut e, &[press('g'), press('g')]);
        // 'a jumps to first non-blank of mark's line.
        feed(&mut e, &[press('\''), press('a')]);
        assert_eq!(e.cursor.line(), 1);
        assert_eq!(e.cursor.col(), 2); // first non-blank
    }

    #[test]
    fn goto_unset_mark_shows_error() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('`'), press('b')]);
        assert!(e.message.is_some());
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("Mark not set"));
    }

    #[test]
    fn multiple_marks() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // Set mark 'a' at line 0, mark 'b' at line 2.
        feed(&mut e, &[press('m'), press('a')]);
        feed(&mut e, &[press('G'), press('m'), press('b')]);
        // Jump to 'a'.
        feed(&mut e, &[press('`'), press('a')]);
        assert_eq!(e.cursor.line(), 0);
        // Jump to 'b'.
        feed(&mut e, &[press('`'), press('b')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn d_tick_mark_linewise() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // Set mark 'a' at line 0.
        feed(&mut e, &[press('m'), press('a')]);
        // Move to line 2, d'a → delete lines 0-2 (linewise to mark line).
        feed(&mut e, &[press('2'), press('j')]);
        feed(&mut e, &[press('d'), press('\''), press('a')]);
        assert_eq!(e.buffer.contents(), "ddd");
    }

    #[test]
    fn d_backtick_mark_charwise() {
        let mut e = editor_with("hello world");
        // Set mark 'a' at col 0.
        feed(&mut e, &[press('m'), press('a')]);
        // Move to col 5 ('_'), d`a → delete from col 0 to col 5 (charwise inclusive).
        feed(&mut e, &[press('4'), press('l')]);
        feed(&mut e, &[press('d'), press('`'), press('a')]);
        assert_eq!(e.buffer.contents(), " world");
    }

    #[test]
    fn mark_persists_across_edits() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // Set mark on line 2.
        feed(&mut e, &[press('G'), press('m'), press('a')]);
        // Go to line 0, insert text.
        feed(&mut e, &[press('g'), press('g'), press('i'), press('x'), esc()]);
        // `a should still go to line 2 (mark position unchanged).
        feed(&mut e, &[press('`'), press('a')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn mark_in_visual_mode() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // Set mark 'a' at line 0.
        feed(&mut e, &[press('m'), press('a')]);
        // Go to line 2, enter visual mode, `a extends selection to mark.
        feed(&mut e, &[press('G'), press('v'), press('`'), press('a')]);
        assert_eq!(e.cursor.line(), 0);
        assert!(e.cursor.has_selection());
    }

    #[test]
    fn m_non_letter_cancels() {
        let mut e = editor_with("hello");
        // m + non-letter should not panic or set any mark.
        feed(&mut e, &[press('m'), press('1')]);
        assert_eq!(e.cursor.col(), 0);
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn zz_in_visual_mode() {
        let mut e = editor_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no");
        e.last_text_height = 10;
        e.view.set_top_line(0);
        // Enter visual mode, move to line 7, zz.
        feed(&mut e, &[press('v'), press('7'), press('j')]);
        feed(&mut e, &[press('z'), press('z')]);
        assert_eq!(e.view.top_line(), 2);
        // Still in visual mode with selection.
        assert!(matches!(e.mode, Mode::Visual(_)));
    }

    // ── Named registers ("x prefix) ─────────────────────────────────────

    #[test]
    fn register_yank_line_into_named() {
        // "ayy — yank line into register a.
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('"'), press('a'), press('y'), press('y')]);
        assert_eq!(e.registers.get(Some('a')).content(), "hello\n");
        // Unnamed also gets it.
        assert_eq!(e.registers.get(None).content(), "hello\n");
    }

    #[test]
    fn register_paste_from_named() {
        // "ayy, j, "ap — yank into a, move down, paste from a.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('"'), press('a'), press('y'), press('y')]);
        feed(&mut e, &[press('j')]);
        feed(&mut e, &[press('"'), press('a'), press('p')]);
        assert_eq!(e.buffer.contents(), "aaa\nbbb\naaa\nccc");
    }

    #[test]
    fn register_delete_into_named() {
        // "add — delete line into register a.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('"'), press('a'), press('d'), press('d')]);
        assert_eq!(e.buffer.contents(), "bbb\nccc");
        assert_eq!(e.registers.get(Some('a')).content(), "aaa\n");
    }

    #[test]
    fn register_x_into_named() {
        // "ax — delete char into register a.
        let mut e = editor_with("hello");
        feed(&mut e, &[press('"'), press('a'), press('x')]);
        assert_eq!(e.buffer.contents(), "ello");
        assert_eq!(e.registers.get(Some('a')).content(), "h");
    }

    #[test]
    fn register_named_isolation() {
        // Different registers don't interfere.
        let mut e = editor_with("alpha\nbravo\ncharlie");
        // "ayy on line 0.
        feed(&mut e, &[press('"'), press('a'), press('y'), press('y')]);
        // j, "byy on line 1.
        feed(&mut e, &[press('j'), press('"'), press('b'), press('y'), press('y')]);
        assert_eq!(e.registers.get(Some('a')).content(), "alpha\n");
        assert_eq!(e.registers.get(Some('b')).content(), "bravo\n");
    }

    #[test]
    fn register_uppercase_appends() {
        // "ayy on line 0, j, "Ayy on line 1 — append line to register a.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('"'), press('a'), press('y'), press('y')]);
        feed(&mut e, &[press('j'), press('"'), press('A'), press('y'), press('y')]);
        assert_eq!(e.registers.get(Some('a')).content(), "aaa\nbbb\n");
    }

    #[test]
    fn register_prefix_with_count() {
        // 3"add — delete 3 lines into register a.
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        feed(&mut e, &[press('3'), press('"'), press('a'), press('d'), press('d')]);
        assert_eq!(e.buffer.contents(), "ddd");
        assert_eq!(e.registers.get(Some('a')).content(), "aaa\nbbb\nccc\n");
    }

    #[test]
    fn register_unnamed_default() {
        // Without "x prefix, yank goes to unnamed only.
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('y'), press('y')]);
        assert_eq!(e.registers.get(None).content(), "hello\n");
        // Named register 'a' is still empty.
        assert!(e.registers.get(Some('a')).is_empty());
    }

    #[test]
    fn register_visual_yank_named() {
        // v$"ay — visual select then yank into register a.
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('v'), press('$')]);
        feed(&mut e, &[press('"'), press('a'), press('y')]);
        assert_eq!(e.registers.get(Some('a')).content(), "hello world");
    }

    #[test]
    fn register_visual_delete_named() {
        // v$"ad — visual select then delete into register a.
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('v'), press('$')]);
        feed(&mut e, &[press('"'), press('a'), press('d')]);
        assert!(e.buffer.contents().is_empty() || e.buffer.contents() == "\n");
        assert_eq!(e.registers.get(Some('a')).content(), "hello world");
    }

    #[test]
    fn register_dw_into_named() {
        // "adw — delete word into register a.
        let mut e = editor_with("hello world");
        feed(&mut e, &[press('"'), press('a'), press('d'), press('w')]);
        assert_eq!(e.buffer.contents(), "world");
        assert_eq!(e.registers.get(Some('a')).content(), "hello ");
    }

    // ── Macros (q/@ recording and replay) ───────────────────────────────

    #[test]
    fn macro_record_and_replay() {
        // qa, A!, Esc, q — record appending "!" to end of line.
        // @a — replay on next line.
        let mut e = editor_with("hello\nworld");
        feed(&mut e, &[press('q'), press('a')]);
        assert!(e.macro_recording.is_some());
        feed(&mut e, &[press('A'), press('!'), esc()]);
        feed(&mut e, &[press('q')]); // Stop recording.
        assert!(e.macro_recording.is_none());
        assert_eq!(e.buffer.contents(), "hello!\nworld");

        // Move down and replay.
        feed(&mut e, &[press('j'), press('@'), press('a')]);
        assert_eq!(e.buffer.contents(), "hello!\nworld!");
    }

    #[test]
    fn macro_replay_with_count() {
        // Record: dd (delete line). Then 2@a to replay twice.
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        feed(&mut e, &[press('q'), press('a')]);
        feed(&mut e, &[press('d'), press('d')]);
        feed(&mut e, &[press('q')]);
        assert_eq!(e.buffer.contents(), "bbb\nccc\nddd");

        // 2@a deletes 2 more lines.
        feed(&mut e, &[press('2'), press('@'), press('a')]);
        assert_eq!(e.buffer.contents(), "ddd");
    }

    #[test]
    fn macro_replay_at_at() {
        // @a then @@ repeats the last macro.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('q'), press('a')]);
        feed(&mut e, &[press('d'), press('d')]);
        feed(&mut e, &[press('q')]);
        // @a deletes one line.
        feed(&mut e, &[press('@'), press('a')]);
        assert_eq!(e.buffer.contents(), "ccc");

        // Reset for a fresh test.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('q'), press('a')]);
        feed(&mut e, &[press('d'), press('d')]);
        feed(&mut e, &[press('q')]);
        feed(&mut e, &[press('@'), press('a')]);
        // @@ repeats the last macro.
        feed(&mut e, &[press('@'), press('@')]);
        assert_eq!(e.buffer.contents(), "");
    }

    #[test]
    fn macro_empty_register_does_nothing() {
        let mut e = editor_with("hello");
        // @b with no recording — does nothing.
        feed(&mut e, &[press('@'), press('b')]);
        assert_eq!(e.buffer.contents(), "hello");
    }

    #[test]
    fn macro_q_stops_recording() {
        let mut e = editor_with("hello");
        feed(&mut e, &[press('q'), press('a')]);
        assert!(e.macro_recording.is_some());
        // Type some keys, then q to stop.
        feed(&mut e, &[press('x')]);
        feed(&mut e, &[press('q')]);
        assert!(e.macro_recording.is_none());
        // The macro should contain the 'x' key but not the stopping 'q'.
        assert_eq!(e.macro_keys[0].len(), 1); // Just 'x'.
    }

    #[test]
    fn macro_does_not_record_during_replay() {
        // After recording, replaying should not modify the macro.
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('q'), press('a')]);
        feed(&mut e, &[press('d'), press('d')]);
        feed(&mut e, &[press('q')]);
        let original_len = e.macro_keys[0].len();

        // Replay — the macro_keys should not grow.
        feed(&mut e, &[press('@'), press('a')]);
        assert_eq!(e.macro_keys[0].len(), original_len);
    }

    #[test]
    fn macro_recording_clears_previous() {
        // Recording into the same register overwrites.
        let mut e = editor_with("hello");
        feed(&mut e, &[press('q'), press('a'), press('x'), press('q')]);
        let len1 = e.macro_keys[0].len();
        assert_eq!(len1, 1);

        // Record again into 'a' with different content.
        feed(&mut e, &[press('q'), press('a'), press('j'), press('j'), press('q')]);
        assert_eq!(e.macro_keys[0].len(), 2); // j, j
    }

    #[test]
    fn macro_separate_registers() {
        // qa and qb are independent.
        let mut e = editor_with("aaa\nbbb\nccc");
        // Record 'a': delete line.
        feed(&mut e, &[press('q'), press('a'), press('d'), press('d'), press('q')]);
        // Record 'b': join line.
        feed(&mut e, &[press('q'), press('b'), press('J'), press('q')]);

        assert_eq!(e.macro_keys[0].len(), 2); // d, d
        assert_eq!(e.macro_keys[1].len(), 1); // J
    }

    #[test]
    fn macro_with_search() {
        // Record a macro that does a search and delete.
        let mut e = editor_with("foo bar baz\nfoo bar baz");
        feed(&mut e, &[press('q'), press('a')]);
        // Search for "bar", then dw.
        feed(&mut e, &[press('/'), press('b'), press('a'), press('r'), enter()]);
        feed(&mut e, &[press('d'), press('w')]);
        feed(&mut e, &[press('q')]);
        assert_eq!(e.buffer.contents(), "foo baz\nfoo bar baz");

        // Replay on the second line.
        feed(&mut e, &[press('@'), press('a')]);
        assert_eq!(e.buffer.contents(), "foo baz\nfoo baz");
    }

    #[test]
    fn macro_no_start_during_replay() {
        // During macro replay, q should not start a new recording.
        let mut e = editor_with("aaa\nbbb");
        // Record a macro that just moves down.
        feed(&mut e, &[press('q'), press('a'), press('j'), press('q')]);
        // During replay, the 'j' replays fine. No new recording starts.
        feed(&mut e, &[press('g'), press('g'), press('@'), press('a')]);
        assert!(e.macro_recording.is_none());
    }

    #[test]
    fn macro_escape_cancels_record_start() {
        // q then Escape should not start recording.
        let mut e = editor_with("hello");
        feed(&mut e, &[press('q'), esc()]);
        assert!(e.macro_recording.is_none());
    }

    #[test]
    fn macro_dot_repeat_inside_macro() {
        // Record: x (delete char) + . (repeat).
        let mut e = editor_with("abcdef");
        feed(&mut e, &[press('q'), press('a')]);
        feed(&mut e, &[press('x')]);  // Delete 'a' → "bcdef"
        feed(&mut e, &[press('.')]);   // Repeat: delete 'b' → "cdef"
        feed(&mut e, &[press('q')]);
        assert_eq!(e.buffer.contents(), "cdef");

        // Replay: should delete 'c' and 'd'.
        feed(&mut e, &[press('@'), press('a')]);
        assert_eq!(e.buffer.contents(), "ef");
    }

    // ── Substitution (:s) ─────────────────────────────────────────────────

    /// Feed a command string (e.g., "s/foo/bar/g") to the editor.
    /// Types `:`, then the command, then Enter.
    fn cmd(editor: &mut Editor, input: &str) {
        let mut events = vec![press(':')];
        for ch in input.chars() {
            events.push(press(ch));
        }
        events.push(enter());
        feed(editor, &events);
    }

    #[test]
    fn sub_basic_current_line() {
        let mut e = editor_with("foo bar foo");
        cmd(&mut e, "s/foo/baz/");
        assert_eq!(e.buffer.contents(), "baz bar foo");
    }

    #[test]
    fn sub_global_flag() {
        let mut e = editor_with("foo bar foo");
        cmd(&mut e, "s/foo/baz/g");
        assert_eq!(e.buffer.contents(), "baz bar baz");
    }

    #[test]
    fn sub_percent_all_lines() {
        let mut e = editor_with("foo\nfoo\nbar");
        cmd(&mut e, "%s/foo/baz/");
        assert_eq!(e.buffer.contents(), "baz\nbaz\nbar");
    }

    #[test]
    fn sub_percent_global() {
        let mut e = editor_with("foo foo\nbar foo\nbaz");
        cmd(&mut e, "%s/foo/x/g");
        assert_eq!(e.buffer.contents(), "x x\nbar x\nbaz");
    }

    #[test]
    fn sub_line_range() {
        let mut e = editor_with("foo\nfoo\nfoo\nfoo");
        cmd(&mut e, "2,3s/foo/bar/");
        assert_eq!(e.buffer.contents(), "foo\nbar\nbar\nfoo");
    }

    #[test]
    fn sub_delete_pattern() {
        // Empty replacement deletes the match.
        let mut e = editor_with("hello world");
        cmd(&mut e, "s/world//");
        assert_eq!(e.buffer.contents(), "hello ");
    }

    #[test]
    fn sub_case_insensitive() {
        let mut e = editor_with("Hello hello HELLO");
        cmd(&mut e, "s/hello/x/gi");
        assert_eq!(e.buffer.contents(), "x x x");
    }

    #[test]
    fn sub_count_only() {
        let mut e = editor_with("foo foo foo");
        cmd(&mut e, "s/foo/bar/gn");
        // `n` flag means don't actually replace.
        assert_eq!(e.buffer.contents(), "foo foo foo");
        // Should show count message.
        assert!(e.message.is_some());
        assert!(e.message.as_deref().unwrap().contains("3 matches"));
    }

    #[test]
    fn sub_regex_pattern() {
        let mut e = editor_with("foo123bar456");
        cmd(&mut e, r"s/[0-9]+/NUM/g");
        assert_eq!(e.buffer.contents(), "fooNUMbarNUM");
    }

    #[test]
    fn sub_capture_groups() {
        let mut e = editor_with("hello world");
        cmd(&mut e, r"s/(\w+) (\w+)/\2 \1/");
        assert_eq!(e.buffer.contents(), "world hello");
    }

    #[test]
    fn sub_ampersand_whole_match() {
        let mut e = editor_with("foo bar");
        cmd(&mut e, "s/foo/[&]/");
        assert_eq!(e.buffer.contents(), "[foo] bar");
    }

    #[test]
    fn sub_alternate_delimiter() {
        let mut e = editor_with("path/to/file");
        cmd(&mut e, "s#path/to#new/dir#");
        assert_eq!(e.buffer.contents(), "new/dir/file");
    }

    #[test]
    fn sub_undo() {
        let mut e = editor_with("foo bar foo");
        cmd(&mut e, "%s/foo/baz/g");
        assert_eq!(e.buffer.contents(), "baz bar baz");
        // Undo should restore everything in one step.
        feed(&mut e, &[press('u')]);
        assert_eq!(e.buffer.contents(), "foo bar foo");
    }

    #[test]
    fn sub_undo_multi_line() {
        let mut e = editor_with("aaa\nbbb\naaa");
        cmd(&mut e, "%s/aaa/xxx/");
        assert_eq!(e.buffer.contents(), "xxx\nbbb\nxxx");
        // One undo reverses all substitutions.
        feed(&mut e, &[press('u')]);
        assert_eq!(e.buffer.contents(), "aaa\nbbb\naaa");
    }

    #[test]
    fn sub_no_match_error() {
        let mut e = editor_with("hello world");
        cmd(&mut e, "s/xyz/abc/");
        // Should show error.
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E486"));
    }

    #[test]
    fn sub_empty_pattern_error() {
        let mut e = editor_with("hello");
        cmd(&mut e, "s//bar/");
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E486"));
    }

    #[test]
    fn sub_repeat_with_s() {
        let mut e = editor_with("foo\nfoo");
        cmd(&mut e, "s/foo/bar/");
        assert_eq!(e.buffer.contents(), "bar\nfoo");
        // Move to next line and repeat.
        feed(&mut e, &[press('j')]);
        cmd(&mut e, "s");
        assert_eq!(e.buffer.contents(), "bar\nbar");
    }

    #[test]
    fn sub_repeat_no_previous_error() {
        let mut e = editor_with("foo");
        cmd(&mut e, "s");
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E33"));
    }

    #[test]
    fn sub_ampersand_normal_mode() {
        let mut e = editor_with("foo bar\nfoo baz");
        cmd(&mut e, "s/foo/x/");
        assert_eq!(e.buffer.contents(), "x bar\nfoo baz");
        // Move to line 2 and press &.
        feed(&mut e, &[press('j'), press('&')]);
        assert_eq!(e.buffer.contents(), "x bar\nx baz");
    }

    #[test]
    fn sub_ampersand_no_previous_error() {
        let mut e = editor_with("foo");
        feed(&mut e, &[press('&')]);
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E33"));
    }

    #[test]
    fn sub_message_multi_line() {
        let mut e = editor_with("foo\nfoo\nfoo");
        cmd(&mut e, "%s/foo/bar/");
        assert!(e.message.is_some());
        let msg = e.message.as_deref().unwrap();
        assert!(msg.contains("3 substitutions"));
        assert!(msg.contains("3 lines"));
    }

    #[test]
    fn sub_message_single_sub_no_message() {
        // Single substitution on a single line shows no message (like Vim).
        let mut e = editor_with("foo bar");
        cmd(&mut e, "s/foo/baz/");
        assert!(e.message.is_none());
    }

    #[test]
    fn sub_visual_range() {
        // Enter visual mode, select lines 2-3, then :s.
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // Go to line 2, enter visual line, go down.
        feed(&mut e, &[press('j')]);
        feed(&mut e, &[press('V'), press('j')]);
        // Press : — should auto-insert '<,'>
        // Then type s/b/x/g and Enter.
        let mut events = vec![press(':')];
        for ch in "s/b/x/g".chars() {
            events.push(press(ch));
        }
        events.push(enter());
        feed(&mut e, &events);
        // Lines 2-3 should be affected.
        assert_eq!(e.buffer.contents(), "aaa\nxxx\nccc\nddd");
    }

    #[test]
    fn sub_cursor_position_after() {
        // Cursor should be at first non-blank of first substituted line.
        let mut e = editor_with("  foo\n  foo\n  bar");
        cmd(&mut e, "%s/foo/baz/");
        assert_eq!(e.cursor.position().line, 0);
        assert_eq!(e.cursor.position().col, 2); // First non-blank.
    }

    #[test]
    fn sub_literal_dollar_in_replacement() {
        // `$` in replacement should be literal, not a capture reference.
        let mut e = editor_with("price: 100");
        cmd(&mut e, "s/100/$200/");
        assert_eq!(e.buffer.contents(), "price: $200");
    }

    #[test]
    fn sub_backslash_n_in_replacement() {
        // `\n` in replacement should insert a newline.
        let mut e = editor_with("hello world");
        cmd(&mut e, r"s/ /\n/");
        assert_eq!(e.buffer.contents(), "hello\nworld");
    }

    #[test]
    fn sub_escaped_ampersand_in_replacement() {
        // `\&` should be a literal `&`.
        let mut e = editor_with("foo bar");
        cmd(&mut e, r"s/foo/a\&b/");
        assert_eq!(e.buffer.contents(), "a&b bar");
    }

    #[test]
    fn sub_backwards_range_error() {
        let mut e = editor_with("aaa\nbbb\nccc");
        cmd(&mut e, "3,1s/a/b/");
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E493"));
    }

    #[test]
    fn sub_invalid_regex_error() {
        let mut e = editor_with("hello");
        cmd(&mut e, "s/[unclosed/bar/");
        assert!(e.message_is_error);
        assert!(e.message.as_deref().unwrap().contains("E486"));
    }

    #[test]
    fn sub_percent_repeat_with_range() {
        // `:s/a/b/` then `:%s` should repeat on all lines.
        let mut e = editor_with("aaa\naaa\naaa");
        cmd(&mut e, "s/aaa/bbb/");
        assert_eq!(e.buffer.contents(), "bbb\naaa\naaa");
        cmd(&mut e, "%s");
        assert_eq!(e.buffer.contents(), "bbb\nbbb\nbbb");
    }

    #[test]
    fn translate_replacement_ampersand() {
        assert_eq!(translate_replacement("&"), "$0");
    }

    #[test]
    fn translate_replacement_capture_groups() {
        assert_eq!(translate_replacement(r"\1 and \2"), "$1 and $2");
    }

    #[test]
    fn translate_replacement_escaped_ampersand() {
        assert_eq!(translate_replacement(r"\&"), "&");
    }

    #[test]
    fn translate_replacement_escaped_backslash() {
        assert_eq!(translate_replacement(r"\\"), "\\");
    }

    #[test]
    fn translate_replacement_newline() {
        assert_eq!(translate_replacement(r"\n"), "\n");
    }

    #[test]
    fn translate_replacement_literal_dollar() {
        assert_eq!(translate_replacement("$100"), "$$100");
    }

    #[test]
    fn translate_replacement_mixed() {
        assert_eq!(translate_replacement(r"[\1] & $"), "[$1] $0 $$");
    }

    // ── Helper: Tab key ───────────────────────────────────────────────────

    /// Create a Tab key press event (Ctrl+I in terminal = jump forward).
    fn tab() -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })
    }

    // ── Jump list (Ctrl+O / Ctrl+I) ──────────────────────────────────────

    #[test]
    fn ctrl_o_after_gg_goes_back() {
        let mut e = editor_with("line0\nline1\nline2\nline3\nline4");
        // Move to line 3.
        feed(&mut e, &[press('3'), press('j')]);
        assert_eq!(e.cursor.line(), 3);
        // gg is a jump — should push line 3 to jump list.
        feed(&mut e, &[press('g'), press('g')]);
        assert_eq!(e.cursor.line(), 0);
        // Ctrl+O goes back to line 3.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 3);
    }

    #[test]
    fn ctrl_i_after_ctrl_o_goes_forward() {
        let mut e = editor_with("line0\nline1\nline2\nline3\nline4");
        feed(&mut e, &[press('3'), press('j')]);
        feed(&mut e, &[press('g'), press('g')]);
        feed(&mut e, &[ctrl('o')]); // back to line 3
        assert_eq!(e.cursor.line(), 3);
        // Tab (Ctrl+I) goes forward to line 0.
        feed(&mut e, &[tab()]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_o_after_search_confirm() {
        let mut e = editor_with("aaa\nbbb\nccc\naaa\nddd");
        // Cursor at line 0. Search for "ccc".
        feed(
            &mut e,
            &[press('/'), press('c'), press('c'), press('c'), enter()],
        );
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O should go back to line 0 (pre-search position).
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_o_after_n_search_next() {
        let mut e = editor_with("aaa\nbbb\naaa\nccc\naaa");
        // Search for "aaa" — incremental search finds it at line 0 (current line).
        feed(
            &mut e,
            &[press('/'), press('a'), press('a'), press('a'), enter()],
        );
        assert_eq!(e.cursor.line(), 0);
        // n jumps to next match (line 2).
        feed(&mut e, &[press('n')]);
        assert_eq!(e.cursor.line(), 2);
        // n again to line 4.
        feed(&mut e, &[press('n')]);
        assert_eq!(e.cursor.line(), 4);
        // Ctrl+O goes back to where we were before the last n (line 2).
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn ctrl_o_after_star_search() {
        let mut e = editor_with("hello world\nfoo bar\nhello again");
        // * searches for word under cursor.
        feed(&mut e, &[press('*')]);
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O goes back to line 0.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_o_after_percent_bracket() {
        let mut e = editor_with("if (true) {\n  x\n}");
        // Move to the opening {.
        feed(&mut e, &[press('f'), press('{')]);
        assert_eq!(e.cursor.line(), 0);
        // % jumps to matching }.
        feed(&mut e, &[press('%')]);
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O goes back to line 0.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_o_after_paragraph_motion() {
        let mut e = editor_with("line1\nline2\n\nline4\nline5\n\nline7");
        // } jumps to next blank line.
        feed(&mut e, &[press('}')]);
        assert_eq!(e.cursor.line(), 2);
        feed(&mut e, &[press('}')]);
        assert_eq!(e.cursor.line(), 5);
        // Ctrl+O goes back to line 2.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn ctrl_o_after_goto_mark() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // Set mark a on line 2.
        feed(&mut e, &[press('2'), press('j'), press('m'), press('a')]);
        // Go to line 0.
        feed(&mut e, &[press('g'), press('g')]);
        // Jump to mark a.
        feed(&mut e, &[press('`'), press('a')]);
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O goes back to line 0 (where we were before `a).
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn ctrl_o_at_start_stays_put() {
        let mut e = editor_with("hello\nworld");
        // No jump history — Ctrl+O should do nothing.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn ctrl_i_at_end_stays_put() {
        let mut e = editor_with("hello\nworld");
        // No forward history — Tab should do nothing.
        feed(&mut e, &[tab()]);
        assert_eq!(e.cursor.line(), 0);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn ctrl_o_multiple_back_and_forward() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9");
        // Make a series of jumps: G, gg, G.
        feed(&mut e, &[press('G')]); // line 9
        feed(&mut e, &[press('g'), press('g')]); // line 0
        feed(&mut e, &[press('G')]); // line 9
        // Now walk back: Ctrl+O × 3.
        feed(&mut e, &[ctrl('o')]); // back to line 0
        assert_eq!(e.cursor.line(), 0);
        feed(&mut e, &[ctrl('o')]); // back to line 9
        assert_eq!(e.cursor.line(), 9);
        // Forward.
        feed(&mut e, &[tab()]); // forward to line 0
        assert_eq!(e.cursor.line(), 0);
        feed(&mut e, &[tab()]); // forward to line 9
        assert_eq!(e.cursor.line(), 9);
    }

    #[test]
    fn new_jump_from_mid_list_truncates_future() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4\nl5");
        // Jump: G (to line 5), gg (to line 0).
        feed(&mut e, &[press('G')]);
        feed(&mut e, &[press('g'), press('g')]);
        // Back one: to line 5.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 5);
        // New jump from here — should truncate the forward history.
        feed(&mut e, &[press('3'), press('G')]);
        assert_eq!(e.cursor.line(), 2);
        // Tab should not go anywhere (future was truncated).
        feed(&mut e, &[tab()]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn ctrl_o_with_count() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9");
        // Make several jumps.
        feed(&mut e, &[press('G')]); // line 9
        feed(&mut e, &[press('g'), press('g')]); // line 0
        feed(&mut e, &[press('G')]); // line 9
        // 2 Ctrl+O — go back 2 steps.
        feed(&mut e, &[press('2'), ctrl('o')]);
        assert_eq!(e.cursor.line(), 9);
    }

    #[test]
    fn ctrl_o_g_big_with_count() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4");
        // G with count: jump to line 3 (1-indexed).
        feed(&mut e, &[press('3'), press('G')]);
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O goes back to line 0.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 0);
    }

    // ── gg as proper prefix key ──────────────────────────────────────────

    #[test]
    fn gg_goto_first_line() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('G')]); // last line
        assert_eq!(e.cursor.line(), 2);
        feed(&mut e, &[press('g'), press('g')]); // first line
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn gg_with_count_goto_line() {
        let mut e = editor_with("l1\nl2\nl3\nl4\nl5");
        feed(&mut e, &[press('3'), press('g'), press('g')]);
        assert_eq!(e.cursor.line(), 2); // 3rd line (0-indexed = 2)
    }

    #[test]
    fn g_prefix_cancel_on_unknown_key() {
        let mut e = editor_with("aaa\nbbb");
        feed(&mut e, &[press('j')]); // line 1
        // g + x = unknown second key, should cancel silently.
        feed(&mut e, &[press('g'), press('x')]);
        assert_eq!(e.cursor.line(), 1); // didn't move
    }

    // ── Change list (g; / g,) ────────────────────────────────────────────

    #[test]
    fn g_semicolon_jumps_to_last_change() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // Make a change on line 1.
        feed(&mut e, &[press('j'), press('i'), press('X'), esc()]);
        // Move somewhere else.
        feed(&mut e, &[press('G')]);
        assert_eq!(e.cursor.line(), 2);
        // g; should jump to line 1 (where the change was).
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 1);
    }

    #[test]
    fn g_comma_jumps_to_newer_change() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        // Change on line 0.
        feed(&mut e, &[press('i'), press('X'), esc()]);
        // Change on line 2.
        feed(&mut e, &[press('2'), press('j'), press('i'), press('Y'), esc()]);
        // g; twice — goes to line 2, then line 0.
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 2);
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 0);
        // g, goes back to newer change (line 2).
        feed(&mut e, &[press('g'), press(',')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn g_semicolon_at_start_shows_error() {
        let mut e = editor_with("aaa");
        // No changes made — g; should show error.
        feed(&mut e, &[press('g'), press(';')]);
        assert!(e.message_is_error);
        assert_eq!(
            e.message.as_deref(),
            Some("E662: At start of changelist")
        );
    }

    #[test]
    fn g_comma_at_end_shows_error() {
        let mut e = editor_with("aaa");
        // Make one change.
        feed(&mut e, &[press('i'), press('X'), esc()]);
        // g; to go back, then g, should show error (at end).
        feed(&mut e, &[press('g'), press(';')]);
        feed(&mut e, &[press('g'), press(',')]);
        assert!(e.message_is_error);
        assert_eq!(
            e.message.as_deref(),
            Some("E663: At end of changelist")
        );
    }

    #[test]
    fn g_semicolon_after_multiple_edits() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4");
        // Edits on multiple lines.
        feed(&mut e, &[press('i'), press('A'), esc()]); // line 0
        feed(&mut e, &[press('j'), press('i'), press('B'), esc()]); // line 1
        feed(
            &mut e,
            &[press('2'), press('j'), press('i'), press('C'), esc()],
        ); // line 3
        // g; walks back through changes.
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 3);
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 1);
        feed(&mut e, &[press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 0);
    }

    #[test]
    fn g_semicolon_with_count() {
        let mut e = editor_with("l0\nl1\nl2\nl3\nl4");
        // Edits on lines 0, 2, 4.
        feed(&mut e, &[press('i'), press('X'), esc()]);
        feed(&mut e, &[press('2'), press('j'), press('i'), press('Y'), esc()]);
        feed(&mut e, &[press('2'), press('j'), press('i'), press('Z'), esc()]);
        // 2g; should go back 2 changes (from line 4 to line 2, then to line 0).
        feed(&mut e, &[press('2'), press('g'), press(';')]);
        assert_eq!(e.cursor.line(), 2);
    }

    // ── gg in visual mode ────────────────────────────────────────────────

    #[test]
    fn gg_in_visual_mode() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('G')]); // line 2
        feed(&mut e, &[press('v')]); // visual char
        feed(&mut e, &[press('g'), press('g')]); // extend to line 0
        assert_eq!(e.cursor.line(), 0);
        assert!(matches!(e.mode, Mode::Visual(VisualKind::Char)));
    }

    // ── Jump list is not populated by non-jump motions ───────────────────

    #[test]
    fn hjkl_does_not_push_jump_list() {
        let mut e = editor_with("aaa\nbbb\nccc");
        feed(&mut e, &[press('j'), press('j')]);
        assert_eq!(e.cursor.line(), 2);
        // Ctrl+O should do nothing — j is not a jump motion.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.line(), 2);
    }

    #[test]
    fn word_motion_does_not_push_jump_list() {
        let mut e = editor_with("one two three four five");
        feed(&mut e, &[press('w'), press('w'), press('w')]);
        // Ctrl+O should do nothing — w is not a jump motion.
        feed(&mut e, &[ctrl('o')]);
        assert_eq!(e.cursor.col(), 14); // still at "four"
    }

    // ── Multi-buffer (:e, :bn, :bp, :bd, :ls, Ctrl+^) ──────────────────

    /// Helper: create a temp file with content, return the path.
    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("n-nvim-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn multi_buf_initial_state() {
        let e = editor_with("hello");
        assert_eq!(e.current_buf_id, 1);
        assert_eq!(e.buf_count(), 1);
        assert!(e.other_bufs.is_empty());
        assert!(e.alternate_buf_id.is_none());
    }

    #[test]
    fn multi_buf_open_file() {
        let path = temp_file("open_test.txt", "file content");
        let mut e = editor_with("original");
        cmd(&mut e, &format!("e {}", path.display()));
        // Now editing the new file.
        assert_eq!(e.buffer.contents(), "file content");
        assert_eq!(e.buf_count(), 2);
        assert_eq!(e.current_buf_id, 2);
        // Original buffer stored in other_bufs.
        assert_eq!(e.other_bufs.len(), 1);
        assert_eq!(e.other_bufs[0].id, 1);
    }

    #[test]
    fn multi_buf_open_already_current() {
        let path = temp_file("already_cur.txt", "same file");
        let mut e = Editor::new();
        e.buffer = Buffer::from_file(&path).unwrap();
        cmd(&mut e, &format!("e {}", path.display()));
        // Should stay on the same buffer, not create a duplicate.
        assert_eq!(e.buf_count(), 1);
    }

    #[test]
    fn multi_buf_open_already_in_other() {
        let path_a = temp_file("other_a.txt", "aaa");
        let path_b = temp_file("other_b.txt", "bbb");
        let mut e = Editor::new();
        e.buffer = Buffer::from_file(&path_a).unwrap();
        // Open file b.
        cmd(&mut e, &format!("e {}", path_b.display()));
        assert_eq!(e.buffer.contents(), "bbb");
        assert_eq!(e.buf_count(), 2);
        // Re-open file a — should switch, not create a third buffer.
        cmd(&mut e, &format!("e {}", path_a.display()));
        assert_eq!(e.buffer.contents(), "aaa");
        assert_eq!(e.buf_count(), 2);
    }

    #[test]
    fn multi_buf_bn_cycles_forward() {
        let path = temp_file("bn_test.txt", "second");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        // Now on buffer 2 ("second"). :bn should go to buffer 1.
        cmd(&mut e, "bn");
        assert_eq!(e.buffer.contents(), "first");
        // :bn again wraps back to buffer 2.
        cmd(&mut e, "bn");
        assert_eq!(e.buffer.contents(), "second");
    }

    #[test]
    fn multi_buf_bp_cycles_backward() {
        let path = temp_file("bp_test.txt", "second");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        // Now on buffer 2. :bp should go to buffer 1.
        cmd(&mut e, "bp");
        assert_eq!(e.buffer.contents(), "first");
        // :bp again wraps to buffer 2.
        cmd(&mut e, "bp");
        assert_eq!(e.buffer.contents(), "second");
    }

    #[test]
    fn multi_buf_bn_single_buffer_error() {
        let mut e = editor_with("only buffer");
        cmd(&mut e, "bn");
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E85")));
    }

    #[test]
    fn multi_buf_bp_single_buffer_error() {
        let mut e = editor_with("only buffer");
        cmd(&mut e, "bp");
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E85")));
    }

    #[test]
    fn multi_buf_bd_closes_current() {
        let path = temp_file("bd_test.txt", "second");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        assert_eq!(e.buf_count(), 2);
        // Close current (second).
        cmd(&mut e, "bd");
        assert_eq!(e.buf_count(), 1);
        assert_eq!(e.buffer.contents(), "first");
    }

    #[test]
    fn multi_buf_bd_refuses_if_modified() {
        let path = temp_file("bd_mod.txt", "original");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        // Modify the buffer.
        feed(&mut e, &[press('i'), press('x'), esc()]);
        cmd(&mut e, "bd");
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E89")));
        assert_eq!(e.buf_count(), 2); // not closed
    }

    #[test]
    fn multi_buf_bd_force_closes_modified() {
        let path = temp_file("bd_force.txt", "original");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        feed(&mut e, &[press('i'), press('x'), esc()]);
        cmd(&mut e, "bd!");
        assert_eq!(e.buf_count(), 1);
        assert_eq!(e.buffer.contents(), "first");
    }

    #[test]
    fn multi_buf_bd_last_buffer_quits() {
        let mut e = editor_with("only buffer");
        cmd(&mut e, "bd");
        // bd on last buffer should return Quit — we verify indirectly:
        // after processing, the editor would quit. Since we use feed(),
        // the Action::Quit is consumed, but we can check state.
        // Actually, let's test the run_command directly.
        let result = e.run_command(Command::BufDelete);
        assert_eq!(result, CommandResult::Quit);
    }

    #[test]
    fn multi_buf_ls_listing() {
        let path = temp_file("ls_test.txt", "second file");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        let listing = e.buf_list();
        // Current buffer should have %a marker.
        assert!(listing.contains("%a"));
        assert!(listing.contains("ls_test.txt"));
        // Buffer 1 should be listed as alternate.
        assert!(listing.contains("[No Name]") || listing.contains("1"));
    }

    #[test]
    fn multi_buf_ctrl_caret_switches() {
        let path = temp_file("caret_test.txt", "second");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        assert_eq!(e.buffer.contents(), "second");
        // Ctrl+^ switches to alternate (first buffer).
        feed(&mut e, &[ctrl('^')]);
        assert_eq!(e.buffer.contents(), "first");
        // Ctrl+^ again switches back.
        feed(&mut e, &[ctrl('^')]);
        assert_eq!(e.buffer.contents(), "second");
    }

    #[test]
    fn multi_buf_ctrl_caret_no_alternate() {
        let mut e = editor_with("only buffer");
        feed(&mut e, &[ctrl('^')]);
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E23")));
    }

    #[test]
    fn multi_buf_preserves_cursor_and_history() {
        let path = temp_file("preserve_test.txt", "second buffer");
        let mut e = editor_with("first buffer content");
        // Move cursor in first buffer.
        feed(&mut e, &[press('w'), press('w')]); // cursor at "content"
        let first_cursor_col = e.cursor.col();
        // Open second file.
        cmd(&mut e, &format!("e {}", path.display()));
        assert_eq!(e.cursor.col(), 0); // new buffer starts at 0,0
        // Switch back.
        feed(&mut e, &[ctrl('^')]);
        assert_eq!(e.cursor.col(), first_cursor_col); // cursor restored
    }

    #[test]
    fn multi_buf_preserves_marks() {
        let path = temp_file("marks_test.txt", "second");
        let mut e = editor_with("line one\nline two\nline three");
        // Set mark 'a' on line 1.
        feed(&mut e, &[press('j'), press('m'), press('a')]);
        assert!(e.marks[0].is_some());
        // Open second file.
        cmd(&mut e, &format!("e {}", path.display()));
        assert!(e.marks[0].is_none()); // new buffer has no marks
        // Switch back.
        feed(&mut e, &[ctrl('^')]);
        assert!(e.marks[0].is_some()); // mark restored
        assert_eq!(e.marks[0].unwrap().line, 1);
    }

    #[test]
    fn multi_buf_preserves_undo_history() {
        let path = temp_file("undo_test.txt", "second");
        let mut e = editor_with("original");
        // Make a change.
        feed(&mut e, &[press('i'), press('x'), esc()]);
        assert_eq!(e.buffer.contents(), "xoriginal");
        // Open second file.
        cmd(&mut e, &format!("e {}", path.display()));
        // Switch back and undo.
        feed(&mut e, &[ctrl('^'), press('u')]);
        assert_eq!(e.buffer.contents(), "original");
    }

    #[test]
    fn multi_buf_switch_resets_mode() {
        let path_b = temp_file("mode_test.txt", "second");
        let path_c = temp_file("mode_test2.txt", "third");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path_b.display()));
        cmd(&mut e, &format!("e {}", path_c.display()));
        // Enter visual mode, then :bn should reset to Normal.
        feed(&mut e, &[press('v')]);
        assert!(matches!(e.mode, Mode::Visual(_)));
        // Escape to normal first (can't type : in visual... actually you can
        // in our editor, but let's test the bn switch properly).
        feed(&mut e, &[esc()]);
        cmd(&mut e, "bn");
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn multi_buf_quit_checks_all() {
        let path = temp_file("quit_test.txt", "second");
        let mut e = editor_with("first");
        cmd(&mut e, &format!("e {}", path.display()));
        // Modify the first buffer (currently in other_bufs).
        feed(&mut e, &[ctrl('^')]); // switch to first
        feed(&mut e, &[press('i'), press('x'), esc()]); // modify
        feed(&mut e, &[ctrl('^')]); // switch back to second
        // :q should refuse — first buffer is modified.
        let result = e.cmd_quit();
        assert!(matches!(result, CommandResult::Err(ref msg) if msg.contains("E37")));
    }

    #[test]
    fn multi_buf_three_buffers() {
        let path_b = temp_file("three_b.txt", "buffer B");
        let path_c = temp_file("three_c.txt", "buffer C");
        let mut e = editor_with("buffer A");
        cmd(&mut e, &format!("e {}", path_b.display()));
        cmd(&mut e, &format!("e {}", path_c.display()));
        assert_eq!(e.buf_count(), 3);
        assert_eq!(e.buffer.contents(), "buffer C");
        // :bn cycles: C → A → B → C
        cmd(&mut e, "bn");
        assert_eq!(e.buffer.contents(), "buffer A");
        cmd(&mut e, "bn");
        assert_eq!(e.buffer.contents(), "buffer B");
        cmd(&mut e, "bn");
        assert_eq!(e.buffer.contents(), "buffer C");
    }

    #[test]
    fn multi_buf_bd_with_three() {
        let path_b = temp_file("three_bd_b.txt", "buffer B");
        let path_c = temp_file("three_bd_c.txt", "buffer C");
        let mut e = editor_with("buffer A");
        cmd(&mut e, &format!("e {}", path_b.display()));
        cmd(&mut e, &format!("e {}", path_c.display()));
        // Close C (current). Should switch to B (alternate).
        cmd(&mut e, "bd");
        assert_eq!(e.buf_count(), 2);
        assert_eq!(e.buffer.contents(), "buffer B");
    }

    #[test]
    fn multi_buf_status_label() {
        let path = temp_file("label_test.txt", "second");
        let mut e = editor_with("first");
        // With one buffer, label is empty.
        assert_eq!(e.buf_info_label(), "");
        // With two buffers, label shows position.
        cmd(&mut e, &format!("e {}", path.display()));
        let label = e.buf_info_label();
        assert!(label.contains("/2"));
    }

    #[test]
    fn multi_buf_open_nonexistent_file_error() {
        let mut e = editor_with("first");
        cmd(&mut e, "e /nonexistent/path/to/file.txt");
        assert!(e.message_is_error);
        assert_eq!(e.buf_count(), 1); // no new buffer created
    }

    #[test]
    fn multi_buf_e_no_path_error() {
        let mut e = editor_with("first");
        cmd(&mut e, "e");
        assert!(e.message_is_error);
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E32") || m.contains("E492")));
    }

    #[test]
    fn multi_buf_alternate_after_bd() {
        let path_b = temp_file("alt_bd_b.txt", "B");
        let path_c = temp_file("alt_bd_c.txt", "C");
        let mut e = editor_with("A");
        cmd(&mut e, &format!("e {}", path_b.display()));
        cmd(&mut e, &format!("e {}", path_c.display()));
        // Alternate is B (last buffer before C).
        // Close C → should go to B.
        cmd(&mut e, "bd");
        assert_eq!(e.buffer.contents(), "B");
    }

    // ── Window splits ────────────────────────────────────────────────────

    #[test]
    fn win_initial_state() {
        let e = editor_with("hello");
        assert_eq!(e.win_count(), 1);
        assert_eq!(e.active_win_id, 1);
        assert!(e.other_wins.is_empty());
        assert_eq!(e.split, Split::leaf(1));
    }

    #[test]
    fn win_sp_creates_horizontal_split() {
        let mut e = editor_with("hello world");
        cmd(&mut e, "sp");
        assert_eq!(e.win_count(), 2);
        // Active window stays the same (window 1).
        assert_eq!(e.active_win_id, 1);
        // New window 2 is in other_wins showing the same buffer.
        assert_eq!(e.other_wins.len(), 1);
        assert_eq!(e.other_wins[0].id, 2);
        assert_eq!(e.other_wins[0].buf_id, e.current_buf_id);
    }

    #[test]
    fn win_vsp_creates_vertical_split() {
        let mut e = editor_with("hello world");
        cmd(&mut e, "vsp");
        assert_eq!(e.win_count(), 2);
        assert_eq!(e.active_win_id, 1);
        assert_eq!(e.other_wins.len(), 1);
        assert_eq!(e.other_wins[0].id, 2);
    }

    #[test]
    fn win_split_aliases() {
        // :split is an alias for :sp.
        let mut e = editor_with("text");
        cmd(&mut e, "split");
        assert_eq!(e.win_count(), 2);

        // :vsplit is an alias for :vsp.
        let mut e2 = editor_with("text");
        cmd(&mut e2, "vsplit");
        assert_eq!(e2.win_count(), 2);
    }

    #[test]
    fn win_ctrl_w_s_splits_horizontally() {
        let mut e = editor_with("hello");
        // Ctrl+W s = :sp
        feed(&mut e, &[ctrl('w'), press('s')]);
        assert_eq!(e.win_count(), 2);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_ctrl_w_v_splits_vertically() {
        let mut e = editor_with("hello");
        // Ctrl+W v = :vsp
        feed(&mut e, &[ctrl('w'), press('v')]);
        assert_eq!(e.win_count(), 2);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_ctrl_w_w_cycles_forward() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        assert_eq!(e.active_win_id, 1);
        // Ctrl+W w cycles to next window.
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.active_win_id, 2);
        // Ctrl+W w again wraps back to window 1.
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_ctrl_w_upper_w_cycles_backward() {
        let mut e = editor_with("hello");
        cmd(&mut e, "vsp");
        // Ctrl+W W cycles backward (from 1 → wraps to 2).
        feed(&mut e, &[ctrl('w'), press('W')]);
        assert_eq!(e.active_win_id, 2);
    }

    #[test]
    fn win_ctrl_w_hjkl_navigates() {
        let mut e = editor_with("hello");
        cmd(&mut e, "vsp");
        // Window 1 is left, window 2 is right. We're on 1.
        // Need to set last_frame_size for neighbor computation.
        e.last_frame_size = (80, 24);
        // Ctrl+W l → move right to window 2.
        feed(&mut e, &[ctrl('w'), press('l')]);
        assert_eq!(e.active_win_id, 2);
        // Ctrl+W h → move left back to window 1.
        feed(&mut e, &[ctrl('w'), press('h')]);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_ctrl_w_jk_navigates_hsplit() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        // Window 1 is top, window 2 is bottom. We're on 1.
        e.last_frame_size = (80, 24);
        // Ctrl+W j → move down to window 2.
        feed(&mut e, &[ctrl('w'), press('j')]);
        assert_eq!(e.active_win_id, 2);
        // Ctrl+W k → move up back to window 1.
        feed(&mut e, &[ctrl('w'), press('k')]);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_ctrl_w_c_closes_window() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        assert_eq!(e.win_count(), 2);
        // Close the active window with Ctrl+W c.
        feed(&mut e, &[ctrl('w'), press('c')]);
        assert_eq!(e.win_count(), 1);
        // The remaining window should be active.
        assert_eq!(e.active_win_id, 2);
    }

    #[test]
    fn win_ctrl_w_c_last_window_error() {
        let mut e = editor_with("hello");
        // Only one window — Ctrl+W c should show error.
        feed(&mut e, &[ctrl('w'), press('c')]);
        assert_eq!(e.win_count(), 1);
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E444")));
    }

    #[test]
    fn win_close_command() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 1);
    }

    #[test]
    fn win_close_clo_alias() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "clo");
        assert_eq!(e.win_count(), 1);
    }

    #[test]
    fn win_close_last_window_error() {
        let mut e = editor_with("hello");
        cmd(&mut e, "close");
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E444")));
        assert_eq!(e.win_count(), 1);
    }

    #[test]
    fn win_ctrl_w_o_closes_all_other_windows() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "vsp");
        assert_eq!(e.win_count(), 3);
        // Ctrl+W o keeps only the active window.
        feed(&mut e, &[ctrl('w'), press('o')]);
        assert_eq!(e.win_count(), 1);
        assert_eq!(e.active_win_id, 1);
        assert!(e.other_wins.is_empty());
    }

    #[test]
    fn win_only_command() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "vsp");
        assert_eq!(e.win_count(), 3);
        cmd(&mut e, "only");
        assert_eq!(e.win_count(), 1);
    }

    #[test]
    fn win_only_on_alias() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "on");
        assert_eq!(e.win_count(), 1);
    }

    #[test]
    fn win_only_already_single_is_noop() {
        let mut e = editor_with("hello");
        cmd(&mut e, "only");
        assert_eq!(e.win_count(), 1);
        // No error message.
        assert!(e.message.is_none() || !e.message_is_error);
    }

    #[test]
    fn win_split_shares_buffer() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        // Both windows show the same buffer.
        assert_eq!(e.current_buf_id, 1);
        assert_eq!(e.other_wins[0].buf_id, 1);
        // Only one buffer exists.
        assert_eq!(e.buf_count(), 1);
    }

    #[test]
    fn win_independent_cursor_position() {
        let mut e = editor_with("hello world\nsecond line");
        cmd(&mut e, "sp");
        // Move cursor in the active window.
        feed(&mut e, &[press('j'), press('l'), press('l')]);
        let active_line = e.cursor.line();
        let active_col = e.cursor.col();
        assert_eq!(active_line, 1);
        assert_eq!(active_col, 2);
        // Switch to the other window — should have the original position.
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.cursor.line(), 0);
        assert_eq!(e.cursor.col(), 0);
    }

    #[test]
    fn win_switch_resets_mode_to_normal() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        // Enter insert mode.
        feed(&mut e, &[press('i')]);
        assert_eq!(e.mode, Mode::Insert);
        // Switch window — should reset to Normal.
        feed(&mut e, &[esc()]); // first exit insert
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn win_close_preserves_buffer() {
        let path = temp_file("win_close_buf.txt", "file content");
        let mut e = editor_with("original");
        cmd(&mut e, &format!("e {}", path.display()));
        // Now on buffer 2. Split.
        cmd(&mut e, "sp");
        assert_eq!(e.win_count(), 2);
        assert_eq!(e.buf_count(), 2);
        // Close the window — buffer should still exist.
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 1);
        assert_eq!(e.buf_count(), 2); // both buffers survive
    }

    #[test]
    fn win_nested_splits() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "vsp");
        assert_eq!(e.win_count(), 3);
        assert_eq!(e.split.leaves().len(), 3);
    }

    #[test]
    fn win_navigate_three_panes() {
        // Create: HSplit(VSplit(1, 3), 2) — window 1 top-left, 3 top-right, 2 bottom.
        let mut e = editor_with("hello");
        cmd(&mut e, "sp"); // HSplit(1, 2)
        cmd(&mut e, "vsp"); // HSplit(VSplit(1, 3), 2)
        e.last_frame_size = (80, 24);
        assert_eq!(e.win_count(), 3);
        assert_eq!(e.active_win_id, 1);
        // Ctrl+W l → right neighbor (window 3).
        feed(&mut e, &[ctrl('w'), press('l')]);
        assert_eq!(e.active_win_id, 3);
        // Ctrl+W j → down neighbor (window 2).
        feed(&mut e, &[ctrl('w'), press('j')]);
        assert_eq!(e.active_win_id, 2);
        // Ctrl+W k → up neighbor (window 3, nearest above).
        feed(&mut e, &[ctrl('w'), press('k')]);
        assert!(e.active_win_id == 1 || e.active_win_id == 3);
    }

    #[test]
    fn win_ctrl_w_escape_cancels() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        // Ctrl+W then Escape should cancel without action.
        feed(&mut e, &[ctrl('w'), esc()]);
        assert_eq!(e.win_count(), 2);
        assert_eq!(e.active_win_id, 1);
    }

    #[test]
    fn win_sp_does_not_conflict_with_substitute() {
        // Regression: :sp should NOT be parsed as a substitution command.
        let mut e = editor_with("hello world");
        cmd(&mut e, "sp");
        assert_eq!(e.win_count(), 2);
        assert!(!e.message_is_error);
    }

    #[test]
    fn win_split_does_not_conflict_with_substitute() {
        // Regression: :split should NOT be parsed as :s + plit.
        let mut e = editor_with("hello world");
        cmd(&mut e, "split");
        assert_eq!(e.win_count(), 2);
        assert!(!e.message_is_error);
    }

    #[test]
    fn win_multiple_close_to_one() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "sp");
        cmd(&mut e, "sp");
        assert_eq!(e.win_count(), 4);
        // Close three times to get back to one.
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 3);
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 2);
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 1);
        // Fourth close should error.
        cmd(&mut e, "close");
        assert!(e.message.as_ref().is_some_and(|m| m.contains("E444")));
    }

    #[test]
    fn win_different_buffers_in_split() {
        let path = temp_file("win_diff_buf.txt", "second buffer");
        let mut e = editor_with("first buffer");
        cmd(&mut e, "vsp");
        // Switch to window 2.
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.active_win_id, 2);
        // Open a different file in window 2.
        cmd(&mut e, &format!("e {}", path.display()));
        assert_eq!(e.buffer.contents(), "second buffer");
        // Switch back to window 1 — should still have original text.
        feed(&mut e, &[ctrl('w'), press('w')]);
        assert_eq!(e.active_win_id, 1);
        assert_eq!(e.buffer.contents(), "first buffer");
    }

    #[test]
    fn win_cycle_with_three_windows() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "vsp");
        assert_eq!(e.win_count(), 3);
        assert_eq!(e.active_win_id, 1);
        // Cycle: 1 → 3 → 2 → 1.
        feed(&mut e, &[ctrl('w'), press('w')]);
        let second = e.active_win_id;
        feed(&mut e, &[ctrl('w'), press('w')]);
        let third = e.active_win_id;
        feed(&mut e, &[ctrl('w'), press('w')]);
        // Should wrap back to 1.
        assert_eq!(e.active_win_id, 1);
        // All three visited different windows.
        assert_ne!(second, third);
        assert_ne!(second, 1);
        assert_ne!(third, 1);
    }

    #[test]
    fn win_only_with_different_buffers() {
        let path = temp_file("win_only_diff.txt", "other");
        let mut e = editor_with("main");
        cmd(&mut e, "vsp");
        feed(&mut e, &[ctrl('w'), press('w')]);
        cmd(&mut e, &format!("e {}", path.display()));
        // Switch back and :only — all windows close, buffers preserved.
        feed(&mut e, &[ctrl('w'), press('w')]);
        cmd(&mut e, "only");
        assert_eq!(e.win_count(), 1);
        // Both buffers should still be accessible.
        assert_eq!(e.buf_count(), 2);
    }

    #[test]
    fn win_close_switches_to_next_window() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp");
        cmd(&mut e, "sp");
        // 3 windows: leaves order [1, 2, 3]. Active = 1.
        assert_eq!(e.active_win_id, 1);
        // Close window 1 → next in cycle.
        cmd(&mut e, "close");
        assert_eq!(e.win_count(), 2);
        // Active should now be a different window.
        assert_ne!(e.active_win_id, 1);
    }

    #[test]
    fn win_ids_monotonically_increase() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp"); // creates win 2
        cmd(&mut e, "sp"); // creates win 3
        let mut ids = e.split.leaves();
        ids.sort();
        // All three IDs present, each unique.
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(e.next_win_id, 4);
    }

    #[test]
    fn win_close_does_not_reuse_ids() {
        let mut e = editor_with("hello");
        cmd(&mut e, "sp"); // win 2
        cmd(&mut e, "close"); // removes win 1 (active), switches to 2
        cmd(&mut e, "sp"); // creates win 3, NOT win 1
        let ids = e.split.leaves();
        assert!(!ids.contains(&1)); // ID 1 never reused.
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }
}
