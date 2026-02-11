//! View — the bridge from buffer to framebuffer.
//!
//! A `View` maps a region of a text [`Buffer`] onto a rectangular area of an
//! n-term [`FrameBuffer`]. It handles:
//!
//! - **Scrolling** — tracks which lines and columns are visible
//! - **Line numbers** — a right-aligned gutter with configurable width
//! - **Tab expansion** — tabs expand to the next tab stop
//! - **Wide characters** — CJK characters consume two terminal columns
//! - **Status line** — mode indicator, filename, cursor position
//! - **Tilde lines** — `~` markers for lines past the end of the buffer
//!
//! # Architecture
//!
//! The View is intentionally lightweight — it holds only scroll state and
//! display configuration. It doesn't own the buffer or cursor; those are
//! passed to [`render`](View::render) as parameters. This makes it easy to
//! associate one view with different buffers (e.g., switching files in a pane).
//!
//! The rendering pipeline:
//!
//! ```text
//! Buffer (ropey)     View          FrameBuffer (n-term)
//! ┌──────────┐   ┌─────────┐    ┌──────────────────┐
//! │ line 0   │   │ scroll  │    │ 1│fn main() {    │
//! │ line 1   │──▶│ gutter  │──▶ │ 2│  println!()  │
//! │ line 2   │   │ tab exp │    │ 3│}              │
//! │ ...      │   │ status  │    │ ~                 │
//! └──────────┘   └─────────┘    │ NORMAL | main.rs  │
//!                               └──────────────────┘
//! ```

use unicode_width::UnicodeWidthChar;

use crate::buffer::Buffer;
use crate::cursor::Cursor;
use crate::mode::{Mode, VisualKind};
use crate::position::Range;
use crate::search;

use n_term::buffer::FrameBuffer;
use n_term::cell::{Attr, Cell, UnderlineStyle};
use n_term::color::CellColor;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute gutter width for line numbers.
///
/// Returns the number of columns needed for right-aligned line numbers plus
/// a separator space. The gutter grows as the line count increases:
///
/// | Lines   | Digits | Gutter |
/// |---------|--------|--------|
/// | 1–9     | 1      | 2      |
/// | 10–99   | 2      | 3      |
/// | 100–999 | 3      | 4      |
///
/// Returns 0 when `show_numbers` is false.
#[must_use]
pub fn gutter_width(line_count: usize, show_numbers: bool) -> u16 {
    if !show_numbers {
        return 0;
    }
    // At least 1, so ilog10 doesn't panic on 0.
    let n = line_count.max(1);
    let digits = n.ilog10() + 1;
    // Safe: digits <= 20 for usize::MAX, well within u16.
    #[allow(clippy::cast_possible_truncation)]
    let width = digits as u16 + 1; // +1 for separator space
    width
}

/// Convert a char column offset to a display column position.
///
/// Walks the character iterator, expanding tabs to the next tab stop and
/// accounting for wide characters (which consume 2 display columns). Stops
/// at `char_col` or when the characters run out.
///
/// This is the key mapping between the buffer's char-based coordinates and
/// the terminal's display-column coordinates. Tabs and CJK characters make
/// these differ.
#[must_use]
pub fn char_col_to_display_col<I: Iterator<Item = char>>(
    chars: I,
    char_col: usize,
    tab_width: u8,
) -> usize {
    let tab_w = tab_width.max(1) as usize;
    let mut display_col = 0;

    for (i, ch) in chars.enumerate() {
        if i >= char_col {
            break;
        }
        match ch {
            '\n' | '\r' => break,
            '\t' => display_col = (display_col / tab_w + 1) * tab_w,
            _ => display_col += ch.width().unwrap_or(0),
        }
    }

    display_col
}

// ---------------------------------------------------------------------------
// Selection helpers
// ---------------------------------------------------------------------------

/// Compute the column range to highlight on a given line for a visual selection.
///
/// `range` is the raw selection from `Cursor::selection()` — ordered, with
/// `end` being the larger position. Both `start` and `end` are **inclusive**
/// (the characters at both positions are selected in Vim visual mode).
///
/// Returns `Some((start_col, end_col))` in half-open notation `[start, end)`
/// for the columns to highlight, or `None` if this line is outside the
/// selection entirely.
fn line_selection_cols(
    range: Range,
    kind: VisualKind,
    line_idx: usize,
) -> Option<(usize, usize)> {
    match kind {
        VisualKind::Char => {
            if line_idx < range.start.line || line_idx > range.end.line {
                return None;
            }

            if range.start.line == range.end.line {
                // Single line: highlight [start.col, end.col] inclusive.
                Some((range.start.col, range.end.col + 1))
            } else if line_idx == range.start.line {
                // First line of multi-line: start.col to end of line.
                Some((range.start.col, usize::MAX))
            } else if line_idx == range.end.line {
                // Last line of multi-line: start of line to end.col inclusive.
                Some((0, range.end.col + 1))
            } else {
                // Middle line: entire line.
                Some((0, usize::MAX))
            }
        }
        VisualKind::Line => {
            // Line-wise: full lines from start to end (both inclusive).
            if line_idx >= range.start.line && line_idx <= range.end.line {
                Some((0, usize::MAX))
            } else {
                None
            }
        }
        VisualKind::Block => {
            // Block: rectangular region. Columns from min to max of
            // start.col and end.col (they may be in either order since
            // Range::ordered sorts by position, not column independently).
            if line_idx < range.start.line || line_idx > range.end.line {
                return None;
            }
            let min_col = range.start.col.min(range.end.col);
            let max_col = range.start.col.max(range.end.col);
            Some((min_col, max_col + 1))
        }
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// A view of a text buffer onto a terminal framebuffer region.
///
/// Tracks scroll position and display configuration. Call
/// [`render`](Self::render) with a buffer, cursor, and target region to
/// paint text on screen.
///
/// The view layout within its assigned area:
///
/// ```text
/// ┌──────┬────────────────────────┐
/// │gutter│      text area         │ ← text_height rows
/// │      │                        │
/// ├──────┴────────────────────────┤
/// │         status line           │ ← 1 row
/// └───────────────────────────────┘
/// ```
#[derive(Debug, Clone)]
pub struct View {
    /// First visible buffer line (0-indexed).
    top_line: usize,

    /// Horizontal scroll offset in display columns.
    left_col: usize,

    /// Whether to show the line number gutter.
    line_numbers: bool,

    /// Tab stop width (display columns per tab stop).
    tab_width: u8,
}

impl Default for View {
    fn default() -> Self {
        Self::new()
    }
}

impl View {
    /// Create a view with default settings: line numbers on, 4-space tabs.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            top_line: 0,
            left_col: 0,
            line_numbers: true,
            tab_width: 4,
        }
    }

    // -- Accessors ----------------------------------------------------------

    /// First visible buffer line (0-indexed).
    #[inline]
    #[must_use]
    pub const fn top_line(&self) -> usize {
        self.top_line
    }

    /// Horizontal scroll offset in display columns.
    #[inline]
    #[must_use]
    pub const fn left_col(&self) -> usize {
        self.left_col
    }

    /// Whether line numbers are shown.
    #[inline]
    #[must_use]
    pub const fn line_numbers(&self) -> bool {
        self.line_numbers
    }

    /// Current tab width.
    #[inline]
    #[must_use]
    pub const fn tab_width(&self) -> u8 {
        self.tab_width
    }

    // -- Configuration ------------------------------------------------------

    /// Enable or disable line numbers.
    pub const fn set_line_numbers(&mut self, show: bool) {
        self.line_numbers = show;
    }

    /// Set the tab stop width (minimum 1).
    pub fn set_tab_width(&mut self, width: u8) {
        self.tab_width = width.max(1);
    }

    /// Set the vertical scroll position directly.
    pub const fn set_top_line(&mut self, line: usize) {
        self.top_line = line;
    }

    /// Set the horizontal scroll position directly.
    pub const fn set_left_col(&mut self, col: usize) {
        self.left_col = col;
    }

    // -- Scrolling ----------------------------------------------------------

    /// Adjust scroll position so the cursor is visible in the viewport.
    ///
    /// Called automatically by [`render`](Self::render). You can also call
    /// it manually to pre-compute the scroll position without rendering.
    pub fn ensure_cursor_visible(
        &mut self,
        cursor: &Cursor,
        buf: &Buffer,
        area_width: u16,
        area_height: u16,
    ) {
        let gw = gutter_width(buf.line_count(), self.line_numbers);
        let text_width = area_width.saturating_sub(gw) as usize;
        let text_height = area_height.saturating_sub(1) as usize; // -1 for status

        if text_height == 0 || text_width == 0 {
            return;
        }

        let cursor_line = cursor.line();

        // Vertical: cursor must be within [top_line, top_line + text_height)
        if cursor_line < self.top_line {
            self.top_line = cursor_line;
        }
        if cursor_line >= self.top_line + text_height {
            self.top_line = cursor_line - text_height + 1;
        }

        // Horizontal: cursor display column must be within [left_col, left_col + text_width)
        let display_col = buf
            .line(cursor_line)
            .map_or(0, |line| {
                char_col_to_display_col(line.chars(), cursor.col(), self.tab_width)
            });

        if display_col < self.left_col {
            self.left_col = display_col;
        }
        if display_col >= self.left_col + text_width {
            self.left_col = display_col - text_width + 1;
        }
    }

    // -- Rendering ----------------------------------------------------------

    /// Render the buffer into the framebuffer.
    ///
    /// Paints line numbers, text content, tilde lines, and a status line
    /// into the rectangular region `(area_x, area_y, area_width, area_height)`
    /// of the given framebuffer.
    ///
    /// `selection` is the visual selection range and kind, if active. The range
    /// comes from `Cursor::selection()` (ordered, both endpoints inclusive).
    /// Pass `None` when not in visual mode.
    ///
    /// Returns the screen position of the cursor as `Some((x, y))` if the
    /// cursor is visible, or `None` if the area is too small.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        buf: &Buffer,
        cursor: &Cursor,
        mode: Mode,
        selection: Option<(Range, VisualKind)>,
        frame: &mut FrameBuffer,
        area_x: u16,
        area_y: u16,
        area_width: u16,
        area_height: u16,
    ) -> Option<(u16, u16)> {
        if area_width == 0 || area_height == 0 {
            return None;
        }

        // Ensure cursor is visible (adjusts scroll).
        self.ensure_cursor_visible(cursor, buf, area_width, area_height);

        let line_count = buf.line_count();
        let gw = gutter_width(line_count, self.line_numbers);
        let text_width = area_width.saturating_sub(gw);
        let text_height = area_height.saturating_sub(1); // status line
        let text_x = area_x + gw;

        let mut cursor_screen: Option<(u16, u16)> = None;

        // -- Text rows and gutter -------------------------------------------

        for row in 0..text_height {
            let screen_y = area_y + row;
            let buf_line = self.top_line + row as usize;

            if buf_line < line_count {
                // Gutter: line number
                if self.line_numbers && gw > 0 {
                    render_line_number(frame, area_x, screen_y, gw, buf_line + 1);
                }

                // Text content (with optional selection highlighting)
                let line_sel = selection.and_then(|(r, k)| line_selection_cols(r, k, buf_line));
                self.render_text_line(frame, buf, buf_line, text_x, screen_y, text_width, line_sel);

                // Cursor screen position
                if buf_line == cursor.line() {
                    let display_col = buf.line(cursor.line()).map_or(0, |line| {
                        char_col_to_display_col(line.chars(), cursor.col(), self.tab_width)
                    });

                    if display_col >= self.left_col {
                        // Safe: offset < text_width which is u16.
                        #[allow(clippy::cast_possible_truncation)]
                        let offset = (display_col - self.left_col) as u16;
                        if offset < text_width {
                            cursor_screen = Some((text_x + offset, screen_y));
                        }
                    }
                }
            } else {
                // Past end of buffer: tilde line
                render_tilde_line(frame, area_x, screen_y, area_width);
            }
        }

        // -- Status line ----------------------------------------------------

        if area_height > 0 {
            let status_y = area_y + text_height;
            render_status_line(frame, buf, cursor, mode, area_x, status_y, area_width);
        }

        cursor_screen
    }

    /// Paint one line of text content into the framebuffer.
    ///
    /// `line_sel` is the optional column range `[start, end)` to highlight
    /// with `Attr::INVERSE` for visual selection. `None` means no selection
    /// on this line.
    #[allow(clippy::too_many_arguments)]
    fn render_text_line(
        &self,
        frame: &mut FrameBuffer,
        buf: &Buffer,
        line_idx: usize,
        x: u16,
        y: u16,
        width: u16,
        line_sel: Option<(usize, usize)>,
    ) {
        let Some(line) = buf.line(line_idx) else {
            fill_empty(frame, x, y, width);
            return;
        };

        let tab_w = self.tab_width.max(1) as usize;
        let left_col = self.left_col;
        let mut display_col: usize = 0;
        let mut screen_col: u16 = 0;
        let mut char_col: usize = 0;

        'chars: for ch in line.chars() {
            // Stop at line endings.
            if ch == '\n' || ch == '\r' {
                break;
            }

            let selected = line_sel
                .is_some_and(|(sel_start, sel_end)| char_col >= sel_start && char_col < sel_end);

            if ch == '\t' {
                // Tab expansion: fill to the next tab stop.
                let next_stop = (display_col / tab_w + 1) * tab_w;
                let spaces = next_stop - display_col;

                for _ in 0..spaces {
                    if display_col >= left_col {
                        if screen_col >= width {
                            break 'chars;
                        }
                        frame.set(x + screen_col, y, sel_cell(' ', selected));
                        screen_col += 1;
                    }
                    display_col += 1;
                }
            } else {
                let char_w = ch.width().unwrap_or(0);
                if char_w == 0 {
                    char_col += 1;
                    continue;
                }

                if display_col >= left_col {
                    if screen_col >= width {
                        break;
                    }

                    if char_w == 2 {
                        // Wide character: needs 2 screen columns.
                        if screen_col + 1 < width {
                            frame.set(x + screen_col, y, sel_cell(ch, selected));
                            frame.set(
                                x + screen_col + 1,
                                y,
                                Cell::continuation(
                                    CellColor::Default,
                                    CellColor::Default,
                                    if selected { Attr::INVERSE } else { Attr::empty() },
                                ),
                            );
                            screen_col += 2;
                        } else {
                            // Wide char doesn't fit — place a space instead.
                            frame.set(x + screen_col, y, sel_cell(' ', selected));
                            screen_col += 1;
                        }
                    } else {
                        frame.set(x + screen_col, y, sel_cell(ch, selected));
                        screen_col += 1;
                    }
                } else if display_col + char_w > left_col {
                    // Wide char straddles the left scroll boundary — the left
                    // half is off-screen, so show a space for the visible part.
                    if screen_col < width {
                        frame.set(x + screen_col, y, sel_cell(' ', selected));
                        screen_col += 1;
                    }
                }

                display_col += char_w;
            }

            char_col += 1;
        }

        // Fill remaining columns. If the selection extends past the line
        // content (e.g., multi-line char-wise or line-wise), highlight the
        // trailing space to show the newline is included.
        let trail_selected =
            line_sel.is_some_and(|(_, sel_end)| char_col < sel_end);
        while screen_col < width {
            frame.set(x + screen_col, y, sel_cell(' ', trail_selected));
            screen_col += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers (stateless, no &self needed)
// ---------------------------------------------------------------------------

/// Render a right-aligned line number in the gutter.
fn render_line_number(
    frame: &mut FrameBuffer,
    x: u16,
    y: u16,
    gutter_w: u16,
    line_num: usize, // 1-indexed
) {
    let num_str = line_num.to_string();
    let digit_space = gutter_w.saturating_sub(1) as usize; // reserve 1 for separator
    let padding = digit_space.saturating_sub(num_str.len());

    let dim_cell = |ch: char| {
        Cell::styled(ch, CellColor::Default, CellColor::Default, Attr::DIM, UnderlineStyle::None)
    };

    let mut col = x;

    // Leading spaces
    for _ in 0..padding {
        frame.set(col, y, dim_cell(' '));
        col += 1;
    }

    // Digits
    for ch in num_str.chars() {
        frame.set(col, y, dim_cell(ch));
        col += 1;
    }

    // Separator space
    if col < x + gutter_w {
        frame.set(col, y, Cell::EMPTY);
    }
}

/// Render a tilde line (past end of buffer).
fn render_tilde_line(frame: &mut FrameBuffer, x: u16, y: u16, width: u16) {
    if width == 0 {
        return;
    }

    // Tilde in blue, matching Vim's NonText highlight group.
    frame.set(
        x,
        y,
        Cell::styled(
            '~',
            CellColor::Ansi256(4), // standard blue
            CellColor::Default,
            Attr::empty(),
            UnderlineStyle::None,
        ),
    );

    // Fill rest of line.
    for col in 1..width {
        frame.set(x + col, y, Cell::EMPTY);
    }
}

/// Render the status line at the bottom of the view.
fn render_status_line(
    frame: &mut FrameBuffer,
    buf: &Buffer,
    cursor: &Cursor,
    mode: Mode,
    x: u16,
    y: u16,
    width: u16,
) {
    if width == 0 {
        return;
    }

    // Left: " MODE | filename [+]"
    let mode_str = mode.display_name();
    let filename = buf
        .path()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("[No Name]");
    let modified = if buf.is_modified() { " [+]" } else { "" };
    let left = format!(" {mode_str} | {filename}{modified}");

    // Right: " line:col "
    let right = format!(" {}:{} ", cursor.line() + 1, cursor.col() + 1);

    let style = Attr::INVERSE;
    // Safe: status line right portion is always short ASCII.
    #[allow(clippy::cast_possible_truncation)]
    let right_len = right.chars().count() as u16;
    let right_start = width.saturating_sub(right_len);

    let mut col: u16 = 0;

    // Left portion (truncated if it would overlap the right).
    for ch in left.chars() {
        if col >= right_start || col >= width {
            break;
        }
        frame.set(
            x + col,
            y,
            Cell::styled(
                ch,
                CellColor::Default,
                CellColor::Default,
                style,
                UnderlineStyle::None,
            ),
        );
        col += 1;
    }

    // Middle fill.
    while col < right_start && col < width {
        frame.set(
            x + col,
            y,
            Cell::styled(
                ' ',
                CellColor::Default,
                CellColor::Default,
                style,
                UnderlineStyle::None,
            ),
        );
        col += 1;
    }

    // Right portion.
    for ch in right.chars() {
        if col >= width {
            break;
        }
        frame.set(
            x + col,
            y,
            Cell::styled(
                ch,
                CellColor::Default,
                CellColor::Default,
                style,
                UnderlineStyle::None,
            ),
        );
        col += 1;
    }
}

/// Create a cell with optional `INVERSE` for visual selection highlighting.
///
/// When `selected` is true the cell gets the `INVERSE` attribute, which
/// swaps foreground and background — the standard Vim highlight for visual
/// selections.
const fn sel_cell(ch: char, selected: bool) -> Cell {
    if selected {
        Cell::styled(
            ch,
            CellColor::Default,
            CellColor::Default,
            Attr::INVERSE,
            UnderlineStyle::None,
        )
    } else {
        Cell::new(ch)
    }
}

/// Highlight search matches in the visible portion of the framebuffer.
///
/// Call this **after** [`View::render`] to paint match highlights over the
/// rendered text. This is a post-processing pass — it reads the existing
/// cell characters and replaces their colors with the search highlight style
/// (black text on yellow background).
///
/// `pattern` is the search string. If empty, this is a no-op.
#[allow(clippy::too_many_arguments)]
pub fn highlight_matches(
    view: &View,
    frame: &mut FrameBuffer,
    buf: &Buffer,
    pattern: &str,
    area_x: u16,
    area_y: u16,
    area_width: u16,
    area_height: u16,
) {
    if pattern.is_empty() || area_height == 0 || area_width == 0 {
        return;
    }

    let gw = gutter_width(buf.line_count(), view.line_numbers);
    let text_x = area_x + gw;
    let text_width = area_width.saturating_sub(gw);
    let text_height = area_height.saturating_sub(1); // status line

    if text_height == 0 || text_width == 0 {
        return;
    }

    let matches = search::find_all(
        buf,
        pattern,
        view.top_line,
        view.top_line + text_height as usize,
    );

    for m in &matches {
        let row = m.start.line.saturating_sub(view.top_line);
        if row >= text_height as usize {
            continue;
        }

        let Some(line) = buf.line(m.start.line) else {
            continue;
        };

        // Compute display column range for the match.
        let match_start_dc = char_col_to_display_col(
            line.chars(),
            m.start.col,
            view.tab_width,
        );
        let match_end_dc = char_col_to_display_col(
            line.chars(),
            m.start.col + m.len,
            view.tab_width,
        );

        // Paint all display columns in [match_start_dc, match_end_dc).
        for dc in match_start_dc..match_end_dc {
            if dc < view.left_col {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            let screen_col = (dc - view.left_col) as u16;
            if screen_col >= text_width {
                break;
            }

            let sx = text_x + screen_col;
            #[allow(clippy::cast_possible_truncation)]
            let sy = area_y + row as u16;

            if let Some(cell) = frame.get(sx, sy) {
                if cell.is_continuation() {
                    frame.set(
                        sx,
                        sy,
                        Cell::continuation(
                            CellColor::Ansi256(0),  // black
                            CellColor::Ansi256(3),  // yellow
                            Attr::empty(),
                        ),
                    );
                } else {
                    let ch = cell.character().unwrap_or(' ');
                    frame.set(
                        sx,
                        sy,
                        Cell::styled(
                            ch,
                            CellColor::Ansi256(0),  // black
                            CellColor::Ansi256(3),  // yellow
                            Attr::BOLD,
                            UnderlineStyle::None,
                        ),
                    );
                }
            }
        }
    }
}

/// Render a search prompt on the bottom line (`/pattern` or `?pattern`).
///
/// Similar to [`render_command_line`] but with a configurable prefix character.
/// Returns the screen position of the search-line cursor.
pub fn render_search_line(
    frame: &mut FrameBuffer,
    prefix: char,
    input: &str,
    cursor_col: usize,
    x: u16,
    y: u16,
    width: u16,
) -> Option<(u16, u16)> {
    if width == 0 {
        return None;
    }

    // Leading prefix ('/' or '?')
    frame.set(x, y, Cell::new(prefix));
    let mut col: u16 = 1;

    // Input text
    for ch in input.chars() {
        if col >= width {
            break;
        }
        frame.set(x + col, y, Cell::new(ch));
        col += 1;
    }

    // Fill remaining with empty cells
    while col < width {
        frame.set(x + col, y, Cell::EMPTY);
        col += 1;
    }

    // Cursor position: after prefix + cursor_col
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = 1 + cursor_col as u16;
    if cursor_x < width {
        Some((x + cursor_x, y))
    } else {
        None
    }
}

/// Fill a span with empty cells.
fn fill_empty(frame: &mut FrameBuffer, x: u16, y: u16, width: u16) {
    for col in 0..width {
        frame.set(x + col, y, Cell::EMPTY);
    }
}

/// Render the command line (`:` prompt with input text).
///
/// Returns the screen position of the command-line cursor as `(x, y)`.
/// The leading `:` is added automatically — `input` should not include it.
pub fn render_command_line(
    frame: &mut FrameBuffer,
    input: &str,
    cursor_col: usize,
    x: u16,
    y: u16,
    width: u16,
) -> Option<(u16, u16)> {
    if width == 0 {
        return None;
    }

    // Leading ':'
    frame.set(x, y, Cell::new(':'));
    let mut col: u16 = 1;

    // Input text
    for ch in input.chars() {
        if col >= width {
            break;
        }
        frame.set(x + col, y, Cell::new(ch));
        col += 1;
    }

    // Fill remaining with empty cells
    while col < width {
        frame.set(x + col, y, Cell::EMPTY);
        col += 1;
    }

    // Cursor position: after ':' + cursor_col
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = 1 + cursor_col as u16;
    if cursor_x < width {
        Some((x + cursor_x, y))
    } else {
        None
    }
}

/// Render a message on the bottom line.
///
/// Error messages are shown with the BOLD attribute and red foreground.
/// Normal messages are shown with default styling.
pub fn render_message_line(
    frame: &mut FrameBuffer,
    message: &str,
    is_error: bool,
    x: u16,
    y: u16,
    width: u16,
) {
    if width == 0 {
        return;
    }

    let mut col: u16 = 0;

    for ch in message.chars() {
        if col >= width {
            break;
        }
        if is_error {
            frame.set(
                x + col,
                y,
                Cell::styled(
                    ch,
                    CellColor::Ansi256(1), // red
                    CellColor::Default,
                    Attr::BOLD,
                    UnderlineStyle::None,
                ),
            );
        } else {
            frame.set(x + col, y, Cell::new(ch));
        }
        col += 1;
    }

    // Fill remaining with empty cells
    while col < width {
        frame.set(x + col, y, Cell::EMPTY);
        col += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::Position;
    use std::path::PathBuf;

    // Helper: create a FrameBuffer and extract a row as characters.
    fn row_chars(frame: &FrameBuffer, y: u16) -> String {
        let row = frame.row(y).unwrap();
        row.iter()
            .filter(|c| !c.is_continuation())
            .map(|c| c.character().unwrap_or(' '))
            .collect()
    }

    // Helper: check if a cell has the INVERSE attribute (for status line).
    fn is_inverse(frame: &FrameBuffer, x: u16, y: u16) -> bool {
        frame
            .get(x, y)
            .is_some_and(|c| c.attrs.contains(Attr::INVERSE))
    }

    // Helper: check if a cell has the DIM attribute (for line numbers).
    fn is_dim(frame: &FrameBuffer, x: u16, y: u16) -> bool {
        frame
            .get(x, y)
            .is_some_and(|c| c.attrs.contains(Attr::DIM))
    }

    // ── gutter_width ──────────────────────────────────────────────────────

    #[test]
    fn gutter_width_single_digit() {
        assert_eq!(gutter_width(1, true), 2); // "1 "
        assert_eq!(gutter_width(9, true), 2); // "9 "
    }

    #[test]
    fn gutter_width_double_digit() {
        assert_eq!(gutter_width(10, true), 3); // "10 "
        assert_eq!(gutter_width(99, true), 3); // "99 "
    }

    #[test]
    fn gutter_width_triple_digit() {
        assert_eq!(gutter_width(100, true), 4); // "100 "
        assert_eq!(gutter_width(999, true), 4); // "999 "
    }

    #[test]
    fn gutter_width_large() {
        assert_eq!(gutter_width(1000, true), 5); // "1000 "
        assert_eq!(gutter_width(10_000, true), 6); // "10000 "
    }

    #[test]
    fn gutter_width_empty_buffer() {
        // Empty buffer still shows line 1.
        assert_eq!(gutter_width(0, true), 2);
    }

    #[test]
    fn gutter_width_disabled() {
        assert_eq!(gutter_width(100, false), 0);
        assert_eq!(gutter_width(1, false), 0);
    }

    // ── char_col_to_display_col ───────────────────────────────────────────

    #[test]
    fn display_col_ascii() {
        // ASCII: 1 char = 1 display column.
        assert_eq!(char_col_to_display_col("hello".chars(), 0, 4), 0);
        assert_eq!(char_col_to_display_col("hello".chars(), 3, 4), 3);
        assert_eq!(char_col_to_display_col("hello".chars(), 5, 4), 5);
    }

    #[test]
    fn display_col_with_tabs() {
        // "\thello" with tab_width=4: tab expands to 4 spaces.
        assert_eq!(char_col_to_display_col("\thello".chars(), 0, 4), 0);
        assert_eq!(char_col_to_display_col("\thello".chars(), 1, 4), 4); // past tab
        assert_eq!(char_col_to_display_col("\thello".chars(), 2, 4), 5); // 'h'
    }

    #[test]
    fn display_col_tab_at_various_positions() {
        // "ab\tcd" with tab_width=4: a(0) b(1) \t→4 c(4) d(5)
        assert_eq!(char_col_to_display_col("ab\tcd".chars(), 2, 4), 2); // on \t
        assert_eq!(char_col_to_display_col("ab\tcd".chars(), 3, 4), 4); // past \t
        assert_eq!(char_col_to_display_col("ab\tcd".chars(), 4, 4), 5); // 'd'
    }

    #[test]
    fn display_col_tab_width_8() {
        assert_eq!(char_col_to_display_col("\thello".chars(), 1, 8), 8);
    }

    #[test]
    fn display_col_with_wide_chars() {
        // "中文hi": 中(2) 文(2) h(1) i(1) = display cols 0-1, 2-3, 4, 5
        assert_eq!(char_col_to_display_col("中文hi".chars(), 0, 4), 0);
        assert_eq!(char_col_to_display_col("中文hi".chars(), 1, 4), 2); // past 中
        assert_eq!(char_col_to_display_col("中文hi".chars(), 2, 4), 4); // past 文
        assert_eq!(char_col_to_display_col("中文hi".chars(), 3, 4), 5); // past h
        assert_eq!(char_col_to_display_col("中文hi".chars(), 4, 4), 6); // past i
    }

    #[test]
    fn display_col_mixed_tabs_and_wide() {
        // "\t中" with tab_width=4: \t→4, 中→4-5
        assert_eq!(char_col_to_display_col("\t中".chars(), 1, 4), 4); // past tab
        assert_eq!(char_col_to_display_col("\t中".chars(), 2, 4), 6); // past 中
    }

    #[test]
    fn display_col_empty_line() {
        assert_eq!(char_col_to_display_col("".chars(), 0, 4), 0);
        assert_eq!(char_col_to_display_col("".chars(), 5, 4), 0);
    }

    #[test]
    fn display_col_stops_at_newline() {
        assert_eq!(char_col_to_display_col("ab\ncd".chars(), 3, 4), 2);
    }

    // ── View::new ─────────────────────────────────────────────────────────

    #[test]
    fn new_defaults() {
        let v = View::new();
        assert_eq!(v.top_line(), 0);
        assert_eq!(v.left_col(), 0);
        assert!(v.line_numbers());
        assert_eq!(v.tab_width(), 4);
    }

    #[test]
    fn default_is_new() {
        let a = View::new();
        let b = View::default();
        assert_eq!(a.top_line(), b.top_line());
        assert_eq!(a.left_col(), b.left_col());
        assert_eq!(a.line_numbers(), b.line_numbers());
        assert_eq!(a.tab_width(), b.tab_width());
    }

    // ── Configuration ─────────────────────────────────────────────────────

    #[test]
    fn set_tab_width_minimum_one() {
        let mut v = View::new();
        v.set_tab_width(0);
        assert_eq!(v.tab_width(), 1);
    }

    #[test]
    fn set_line_numbers_toggle() {
        let mut v = View::new();
        assert!(v.line_numbers());
        v.set_line_numbers(false);
        assert!(!v.line_numbers());
    }

    #[test]
    fn set_scroll_position() {
        let mut v = View::new();
        v.set_top_line(10);
        v.set_left_col(5);
        assert_eq!(v.top_line(), 10);
        assert_eq!(v.left_col(), 5);
    }

    // ── ensure_cursor_visible ─────────────────────────────────────────────

    #[test]
    fn scroll_cursor_already_visible() {
        let buf = Buffer::from_text("one\ntwo\nthree\nfour\nfive");
        let cursor = Cursor::at(Position::new(1, 0));
        let mut v = View::new();

        v.ensure_cursor_visible(&cursor, &buf, 40, 10);
        assert_eq!(v.top_line(), 0); // no scroll needed
    }

    #[test]
    fn scroll_down_when_cursor_below() {
        let buf = Buffer::from_text("one\ntwo\nthree\nfour\nfive");
        let cursor = Cursor::at(Position::new(4, 0));
        let mut v = View::new();

        // area_height=4: text_height=3 (minus status). Lines 0,1,2 visible.
        // Cursor at line 4 → need to scroll.
        v.ensure_cursor_visible(&cursor, &buf, 40, 4);
        assert_eq!(v.top_line(), 2); // lines 2,3,4 visible
    }

    #[test]
    fn scroll_up_when_cursor_above() {
        let buf = Buffer::from_text("one\ntwo\nthree\nfour\nfive");
        let cursor = Cursor::at(Position::new(0, 0));
        let mut v = View::new();
        v.set_top_line(3);

        v.ensure_cursor_visible(&cursor, &buf, 40, 4);
        assert_eq!(v.top_line(), 0);
    }

    #[test]
    fn scroll_right_for_long_line() {
        let buf = Buffer::from_text(&"a".repeat(100));
        let cursor = Cursor::at(Position::new(0, 50));
        let mut v = View::new();

        // gutter_width for 1 line = 2. text_width = 20 - 2 = 18.
        v.ensure_cursor_visible(&cursor, &buf, 20, 3);
        // cursor at display_col 50 needs left_col >= 50 - 18 + 1 = 33
        assert_eq!(v.left_col(), 33);
    }

    #[test]
    fn scroll_left_when_cursor_before() {
        let buf = Buffer::from_text(&"a".repeat(100));
        let cursor = Cursor::at(Position::new(0, 5));
        let mut v = View::new();
        v.set_left_col(20);

        v.ensure_cursor_visible(&cursor, &buf, 20, 3);
        assert_eq!(v.left_col(), 5);
    }

    #[test]
    fn scroll_noop_zero_size() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut v = View::new();
        v.set_top_line(5);

        // Zero-size area: no adjustment.
        v.ensure_cursor_visible(&cursor, &buf, 0, 0);
        assert_eq!(v.top_line(), 5);
    }

    // ── render — zero/small areas ─────────────────────────────────────────

    #[test]
    fn render_zero_area_returns_none() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(80, 24);
        let mut v = View::new();

        assert!(v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 0, 0).is_none());
        assert!(v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 0, 5).is_none());
        assert!(v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 5, 0).is_none());
    }

    #[test]
    fn render_one_row_only_status() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 1);
        let mut v = View::new();

        // height=1 → text_height=0, only status line.
        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 1);
        assert!(pos.is_none()); // no text rows, cursor not placed

        // Status line should be rendered on row 0.
        assert!(is_inverse(&frame, 0, 0));
    }

    // ── render — line numbers ─────────────────────────────────────────────

    #[test]
    fn render_line_numbers_appear() {
        let buf = Buffer::from_text("one\ntwo\nthree");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 5);

        // Gutter is 2 wide for 3 lines: "1 ", "2 ", "3 "
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with("1 "), "row0 = '{row0}'");
        let row1 = row_chars(&frame, 1);
        assert!(row1.starts_with("2 "), "row1 = '{row1}'");
        let row2 = row_chars(&frame, 2);
        assert!(row2.starts_with("3 "), "row2 = '{row2}'");
    }

    #[test]
    fn render_line_numbers_right_aligned() {
        // 10+ lines: gutter = 3 columns.
        let text: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let buf = Buffer::from_text(text.trim_end());
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 14);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 14);

        // Line 1: " 1 " (space, digit, space)
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with(" 1 "), "row0 = '{row0}'");
        // Line 10: "10 " (two digits, space)
        let row9 = row_chars(&frame, 9);
        assert!(row9.starts_with("10 "), "row9 = '{row9}'");
    }

    #[test]
    fn render_line_numbers_are_dim() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // Line number "1" at column 0 should be DIM.
        assert!(is_dim(&frame, 0, 0));
    }

    #[test]
    fn render_no_gutter_when_disabled() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();
        v.set_line_numbers(false);

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // First column should be text, not a line number.
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with('h'), "row0 = '{row0}'");
    }

    // ── render — text content ─────────────────────────────────────────────

    #[test]
    fn render_basic_text() {
        let buf = Buffer::from_text("hello world");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // Gutter = 2, text starts at col 2.
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with("1 hello world"), "row0 = '{row0}'");
    }

    #[test]
    fn render_tab_expansion() {
        let buf = Buffer::from_text("\thello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // Tab should expand to 4 spaces: "1     hello" (gutter "1 " + 4 spaces + "hello")
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with("1     hello"), "row0 = '{row0}'");
    }

    #[test]
    fn render_wide_characters() {
        let buf = Buffer::from_text("中文hi");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // After gutter "1 ": 中 takes 2 cols, 文 takes 2 cols, h=1, i=1.
        // Check the main characters (skipping continuations).
        let row = frame.row(0).unwrap();
        // col 2 = '中', col 3 = continuation, col 4 = '文', col 5 = continuation,
        // col 6 = 'h', col 7 = 'i'
        assert_eq!(row[2].character(), Some('中'));
        assert!(row[3].is_continuation());
        assert_eq!(row[4].character(), Some('文'));
        assert!(row[5].is_continuation());
        assert_eq!(row[6].character(), Some('h'));
        assert_eq!(row[7].character(), Some('i'));
    }

    #[test]
    fn render_fills_remaining_columns() {
        let buf = Buffer::from_text("hi");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(10, 3);
        let mut v = View::new();

        // Pre-fill with 'X' to detect which cells were written.
        for y in 0..3u16 {
            for x in 0..10u16 {
                frame.set(x, y, Cell::new('X'));
            }
        }

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 10, 3);

        // After "1 hi" (4 cols), remaining cols should be EMPTY (space).
        let row = frame.row(0).unwrap();
        for col in 4..10 {
            assert_eq!(
                row[col].character(),
                Some(' '),
                "col {col} should be space"
            );
        }
    }

    #[test]
    fn render_empty_buffer() {
        let buf = Buffer::new();
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 5);

        // Line 1 should show "1 " then empty text.
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with("1 "), "row0 = '{row0}'");

        // Lines 2-3 should be tildes.
        assert_eq!(frame.get(0, 1).unwrap().character(), Some('~'));
        assert_eq!(frame.get(0, 2).unwrap().character(), Some('~'));
        assert_eq!(frame.get(0, 3).unwrap().character(), Some('~'));
    }

    #[test]
    fn render_multiline() {
        let buf = Buffer::from_text("aaa\nbbb\nccc");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 5);

        let row0 = row_chars(&frame, 0);
        let row1 = row_chars(&frame, 1);
        let row2 = row_chars(&frame, 2);
        assert!(row0.starts_with("1 aaa"), "row0 = '{row0}'");
        assert!(row1.starts_with("2 bbb"), "row1 = '{row1}'");
        assert!(row2.starts_with("3 ccc"), "row2 = '{row2}'");
    }

    // ── render — tilde lines ──────────────────────────────────────────────

    #[test]
    fn render_tildes_after_buffer() {
        let buf = Buffer::from_text("only one line");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 5);

        // Row 0: text. Rows 1-3: tildes. Row 4: status.
        assert_eq!(frame.get(0, 1).unwrap().character(), Some('~'));
        assert_eq!(frame.get(0, 2).unwrap().character(), Some('~'));
        assert_eq!(frame.get(0, 3).unwrap().character(), Some('~'));
    }

    #[test]
    fn render_tilde_is_blue() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 4);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);

        // Tilde on row 1 should have Ansi256(4) foreground.
        let tilde_cell = frame.get(0, 1).unwrap();
        assert_eq!(tilde_cell.fg, CellColor::Ansi256(4));
    }

    // ── render — status line ──────────────────────────────────────────────

    #[test]
    fn status_line_shows_mode() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 3);

        let status = row_chars(&frame, 2);
        assert!(status.contains("NORMAL"), "status = '{status}'");
    }

    #[test]
    fn status_line_shows_insert_mode() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Insert, None, &mut frame, 0, 0, 40, 3);

        let status = row_chars(&frame, 2);
        assert!(status.contains("INSERT"), "status = '{status}'");
    }

    #[test]
    fn status_line_shows_filename() {
        let mut buf = Buffer::from_text("hello");
        buf.set_path(PathBuf::from("/home/user/main.rs"));
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 3);

        let status = row_chars(&frame, 2);
        assert!(status.contains("main.rs"), "status = '{status}'");
    }

    #[test]
    fn status_line_shows_no_name() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 3);

        let status = row_chars(&frame, 2);
        assert!(status.contains("[No Name]"), "status = '{status}'");
    }

    #[test]
    fn status_line_shows_modified() {
        let mut buf = Buffer::from_text("hello");
        buf.insert(Position::ZERO, "x");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 3);

        let status = row_chars(&frame, 2);
        assert!(status.contains("[+]"), "status = '{status}'");
    }

    #[test]
    fn status_line_shows_cursor_position() {
        let buf = Buffer::from_text("hello\nworld");
        let cursor = Cursor::at(Position::new(1, 3));
        let mut frame = FrameBuffer::new(40, 4);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 40, 4);

        let status = row_chars(&frame, 3);
        // Position is 1-indexed: line 2, col 4.
        assert!(status.contains("2:4"), "status = '{status}'");
    }

    #[test]
    fn status_line_is_inverse() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // All cells on the status row should have INVERSE.
        for x in 0..20 {
            assert!(is_inverse(&frame, x, 2), "col {x} not inverse");
        }
    }

    // ── render — cursor position ──────────────────────────────────────────

    #[test]
    fn cursor_at_origin() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // Gutter = 2, cursor at (2, 0).
        assert_eq!(pos, Some((2, 0)));
    }

    #[test]
    fn cursor_in_middle() {
        let buf = Buffer::from_text("hello\nworld");
        let cursor = Cursor::at(Position::new(1, 3));
        let mut frame = FrameBuffer::new(20, 4);
        let mut v = View::new();

        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);

        // Gutter = 2, cursor at line 1 col 3 → screen (2+3, 1) = (5, 1).
        assert_eq!(pos, Some((5, 1)));
    }

    #[test]
    fn cursor_with_offset_area() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 24);
        let mut v = View::new();

        // Render in a sub-region starting at (10, 5).
        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 10, 5, 20, 3);

        // Gutter = 2 → cursor at (10+2, 5) = (12, 5).
        assert_eq!(pos, Some((12, 5)));
    }

    #[test]
    fn cursor_with_scroll() {
        let buf = Buffer::from_text("one\ntwo\nthree\nfour\nfive");
        let cursor = Cursor::at(Position::new(4, 2));
        let mut frame = FrameBuffer::new(20, 4);
        let mut v = View::new();

        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);

        // text_height = 3. Cursor at line 4 → top_line = 2.
        // Screen row = 4 - 2 = 2.
        // Gutter = 2 (5 lines = 1 digit). Cursor col 2 → screen x = 2 + 2 = 4.
        assert_eq!(pos, Some((4, 2)));
        assert_eq!(v.top_line(), 2);
    }

    #[test]
    fn cursor_with_tab() {
        let buf = Buffer::from_text("\thello");
        let cursor = Cursor::at(Position::new(0, 1)); // on 'h', past the tab
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        // Tab expands to 4 display columns. Cursor char col 1 → display col 4.
        // Gutter = 2. Screen x = 2 + 4 = 6.
        assert_eq!(pos, Some((6, 0)));
    }

    // ── render — scrolling behavior ───────────────────────────────────────

    #[test]
    fn vertical_scroll_shows_correct_lines() {
        let buf = Buffer::from_text("zero\none\ntwo\nthree\nfour");
        let cursor = Cursor::at(Position::new(3, 0));
        let mut frame = FrameBuffer::new(20, 4);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);

        // text_height = 3. Cursor at line 3 → top_line = 1.
        let row0 = row_chars(&frame, 0);
        let row1 = row_chars(&frame, 1);
        let row2 = row_chars(&frame, 2);
        assert!(row0.contains("one"), "row0 = '{row0}'");
        assert!(row1.contains("two"), "row1 = '{row1}'");
        assert!(row2.contains("three"), "row2 = '{row2}'");
    }

    #[test]
    fn horizontal_scroll_shows_correct_content() {
        let buf = Buffer::from_text("abcdefghijklmnop");
        let cursor = Cursor::at(Position::new(0, 14));
        let mut frame = FrameBuffer::new(10, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 10, 3);

        // gutter = 2, text_width = 8. cursor at display_col 14.
        // left_col = 14 - 8 + 1 = 7. First visible char is 'h' (index 7).
        let row = frame.row(0).unwrap();
        assert_eq!(row[2].character(), Some('h')); // text_x = 2
        assert_eq!(row[3].character(), Some('i'));
    }

    #[test]
    fn scroll_follows_cursor_movement() {
        let buf = Buffer::from_text("one\ntwo\nthree\nfour\nfive");
        let mut v = View::new();
        let mut frame = FrameBuffer::new(20, 4);

        // Start at top.
        let cursor = Cursor::new();
        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);
        assert_eq!(v.top_line(), 0);

        // Move cursor down past viewport.
        let cursor = Cursor::at(Position::new(4, 0));
        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);
        assert_eq!(v.top_line(), 2);

        // Move cursor back to top.
        let cursor = Cursor::at(Position::new(0, 0));
        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 4);
        assert_eq!(v.top_line(), 0);
    }

    // ── render — edge cases ───────────────────────────────────────────────

    #[test]
    fn render_single_char_buffer() {
        let buf = Buffer::from_text("x");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(10, 3);
        let mut v = View::new();

        let pos = v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 10, 3);

        assert_eq!(pos, Some((2, 0)));
        let row0 = row_chars(&frame, 0);
        assert!(row0.starts_with("1 x"), "row0 = '{row0}'");
    }

    #[test]
    fn render_line_with_only_newline() {
        let buf = Buffer::from_text("\n\n");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(10, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 10, 5);

        // 3 lines (two \n + trailing empty). All should have line numbers.
        let row0 = row_chars(&frame, 0);
        let row1 = row_chars(&frame, 1);
        let row2 = row_chars(&frame, 2);
        assert!(row0.starts_with("1 "), "row0 = '{row0}'");
        assert!(row1.starts_with("2 "), "row1 = '{row1}'");
        assert!(row2.starts_with("3 "), "row2 = '{row2}'");
    }

    #[test]
    fn render_narrow_viewport_clips_text() {
        let buf = Buffer::from_text("hello world");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(6, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 6, 3);

        // gutter = 2, text_width = 4. Only "hell" visible.
        let row = frame.row(0).unwrap();
        assert_eq!(row[2].character(), Some('h'));
        assert_eq!(row[3].character(), Some('e'));
        assert_eq!(row[4].character(), Some('l'));
        assert_eq!(row[5].character(), Some('l'));
    }

    #[test]
    fn render_wide_char_at_right_edge() {
        // If a wide char doesn't fit at the right edge, a space is placed.
        let buf = Buffer::from_text("ab中");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(7, 3); // gutter=2, text_width=5
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 7, 3);

        // "ab中" = a(1) b(1) 中(2) = 4 cols. Fits in 5 cols.
        let row = frame.row(0).unwrap();
        assert_eq!(row[2].character(), Some('a'));
        assert_eq!(row[3].character(), Some('b'));
        assert_eq!(row[4].character(), Some('中'));
        assert!(row[5].is_continuation());
    }

    // ── render_command_line ──────────────────────────────────────────────

    #[test]
    fn command_line_basic() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_command_line(&mut frame, "wq", 2, 0, 0, 20);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with(":wq"), "row = '{row}'");
        // Cursor after "wq" → col 3 (: + w + q)
        assert_eq!(pos, Some((3, 0)));
    }

    #[test]
    fn command_line_empty_input() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_command_line(&mut frame, "", 0, 0, 0, 20);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with(':'), "row = '{row}'");
        // Cursor right after ':'
        assert_eq!(pos, Some((1, 0)));
    }

    #[test]
    fn command_line_cursor_in_middle() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_command_line(&mut frame, "write", 2, 0, 0, 20);

        // Cursor at char offset 2 → screen col 3 (: + w + r)
        assert_eq!(pos, Some((3, 0)));
    }

    #[test]
    fn command_line_zero_width() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_command_line(&mut frame, "wq", 0, 0, 0, 0);
        assert!(pos.is_none());
    }

    #[test]
    fn command_line_with_offset() {
        let mut frame = FrameBuffer::new(40, 24);
        let pos = render_command_line(&mut frame, "q!", 2, 5, 10, 20);

        // x offset = 5, cursor at 5 + 1 + 2 = 8
        assert_eq!(pos, Some((8, 10)));
    }

    #[test]
    fn command_line_fills_remaining() {
        let mut frame = FrameBuffer::new(10, 1);

        // Pre-fill with X to detect clearing.
        for col in 0..10u16 {
            frame.set(col, 0, Cell::new('X'));
        }

        render_command_line(&mut frame, "w", 1, 0, 0, 10);

        // ":w" occupies cols 0-1, rest should be empty (space).
        let row = frame.row(0).unwrap();
        assert_eq!(row[0].character(), Some(':'));
        assert_eq!(row[1].character(), Some('w'));
        for col in 2..10 {
            assert_eq!(row[col].character(), Some(' '), "col {col} not cleared");
        }
    }

    // ── render_message_line ──────────────────────────────────────────────

    #[test]
    fn message_line_normal() {
        let mut frame = FrameBuffer::new(30, 1);
        render_message_line(&mut frame, "written 42B", false, 0, 0, 30);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with("written 42B"), "row = '{row}'");

        // Normal message: should NOT be bold.
        let cell = frame.get(0, 0).unwrap();
        assert!(!cell.attrs.contains(Attr::BOLD));
    }

    #[test]
    fn message_line_error() {
        let mut frame = FrameBuffer::new(30, 1);
        render_message_line(&mut frame, "E37: No write", true, 0, 0, 30);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with("E37: No write"), "row = '{row}'");

        // Error message: should be bold + red.
        let cell = frame.get(0, 0).unwrap();
        assert!(cell.attrs.contains(Attr::BOLD));
        assert_eq!(cell.fg, CellColor::Ansi256(1));
    }

    #[test]
    fn message_line_empty() {
        let mut frame = FrameBuffer::new(10, 1);

        for col in 0..10u16 {
            frame.set(col, 0, Cell::new('X'));
        }

        render_message_line(&mut frame, "", false, 0, 0, 10);

        // All cells should be empty (space).
        for col in 0..10 {
            assert_eq!(
                frame.get(col, 0).unwrap().character(),
                Some(' '),
                "col {col} not cleared"
            );
        }
    }

    #[test]
    fn message_line_zero_width() {
        let mut frame = FrameBuffer::new(20, 1);
        // Should not panic.
        render_message_line(&mut frame, "hello", false, 0, 0, 0);
    }

    #[test]
    fn message_line_truncates() {
        let mut frame = FrameBuffer::new(5, 1);
        render_message_line(&mut frame, "hello world", false, 0, 0, 5);

        let row = row_chars(&frame, 0);
        assert_eq!(row, "hello");
    }

    // ── line_selection_cols ───────────────────────────────────────────────

    #[test]
    fn sel_char_single_line() {
        // Cursor at (0,2), anchor at (0,0): range start=(0,0) end=(0,2).
        let range = Range::new(Position::new(0, 0), Position::new(0, 2));
        // Chars 0, 1, 2 selected → cols [0, 3).
        assert_eq!(
            line_selection_cols(range, VisualKind::Char, 0),
            Some((0, 3))
        );
        // Other lines: nothing.
        assert_eq!(line_selection_cols(range, VisualKind::Char, 1), None);
    }

    #[test]
    fn sel_char_multi_line() {
        // Selection from (1,3) to (3,1).
        let range = Range::new(Position::new(1, 3), Position::new(3, 1));

        assert_eq!(line_selection_cols(range, VisualKind::Char, 0), None);
        // First line: from col 3 to EOL.
        assert_eq!(
            line_selection_cols(range, VisualKind::Char, 1),
            Some((3, usize::MAX))
        );
        // Middle line: full.
        assert_eq!(
            line_selection_cols(range, VisualKind::Char, 2),
            Some((0, usize::MAX))
        );
        // Last line: 0 to col 1 inclusive → [0, 2).
        assert_eq!(
            line_selection_cols(range, VisualKind::Char, 3),
            Some((0, 2))
        );
        assert_eq!(line_selection_cols(range, VisualKind::Char, 4), None);
    }

    #[test]
    fn sel_line_mode() {
        let range = Range::new(Position::new(1, 5), Position::new(3, 0));

        assert_eq!(line_selection_cols(range, VisualKind::Line, 0), None);
        assert_eq!(
            line_selection_cols(range, VisualKind::Line, 1),
            Some((0, usize::MAX))
        );
        assert_eq!(
            line_selection_cols(range, VisualKind::Line, 2),
            Some((0, usize::MAX))
        );
        assert_eq!(
            line_selection_cols(range, VisualKind::Line, 3),
            Some((0, usize::MAX))
        );
        assert_eq!(line_selection_cols(range, VisualKind::Line, 4), None);
    }

    #[test]
    fn sel_block_mode() {
        // Block from (1,5) to (3,2) → cols [2, 6) on lines 1-3.
        let range = Range::new(Position::new(1, 5), Position::new(3, 2));

        assert_eq!(line_selection_cols(range, VisualKind::Block, 0), None);
        assert_eq!(
            line_selection_cols(range, VisualKind::Block, 1),
            Some((2, 6))
        );
        assert_eq!(
            line_selection_cols(range, VisualKind::Block, 2),
            Some((2, 6))
        );
        assert_eq!(
            line_selection_cols(range, VisualKind::Block, 3),
            Some((2, 6))
        );
        assert_eq!(line_selection_cols(range, VisualKind::Block, 4), None);
    }

    #[test]
    fn sel_char_single_char() {
        // Single char selection: anchor == cursor.
        let range = Range::new(Position::new(2, 4), Position::new(2, 4));
        // One char: [4, 5).
        assert_eq!(
            line_selection_cols(range, VisualKind::Char, 2),
            Some((4, 5))
        );
    }

    // ── render with selection ────────────────────────────────────────────

    #[test]
    fn render_char_selection_single_line() {
        let buf = Buffer::from_text("hello world");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        // Select chars 2-4: "llo"
        let sel = Some((
            Range::new(Position::new(0, 2), Position::new(0, 4)),
            VisualKind::Char,
        ));
        v.render(&buf, &cursor, Mode::Normal, sel, &mut frame, 0, 0, 20, 3);

        // Gutter = 2. Text starts at col 2.
        // Chars 0-1 ('h','e') at screen cols 2-3: NOT inverse.
        assert!(!is_inverse(&frame, 2, 0)); // 'h'
        assert!(!is_inverse(&frame, 3, 0)); // 'e'
        // Chars 2-4 ('l','l','o') at screen cols 4-6: INVERSE.
        assert!(is_inverse(&frame, 4, 0)); // 'l'
        assert!(is_inverse(&frame, 5, 0)); // 'l'
        assert!(is_inverse(&frame, 6, 0)); // 'o'
        // Char 5 (' ') at screen col 7: NOT inverse.
        assert!(!is_inverse(&frame, 7, 0));
    }

    #[test]
    fn render_line_selection() {
        let buf = Buffer::from_text("one\ntwo\nthree");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        // Select lines 0-1 with line-wise mode.
        let sel = Some((
            Range::new(Position::new(0, 0), Position::new(1, 2)),
            VisualKind::Line,
        ));
        v.render(&buf, &cursor, Mode::Normal, sel, &mut frame, 0, 0, 20, 5);

        // Line 0 text area should be inverse (gutter is not).
        let gw = gutter_width(3, true) as usize;
        assert!(is_inverse(&frame, gw as u16, 0)); // first text char, line 0
        assert!(is_inverse(&frame, gw as u16, 1)); // first text char, line 1
        // Trailing cells should also be inverse (line-wise highlights to edge).
        assert!(is_inverse(&frame, 19, 0));
        assert!(is_inverse(&frame, 19, 1));
        // Line 2 should NOT be inverse.
        assert!(!is_inverse(&frame, gw as u16, 2));
    }

    #[test]
    fn render_char_selection_multi_line_trailing() {
        let buf = Buffer::from_text("abc\ndef\nghi");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        // Char-wise from (0,1) to (1,1): selects "bc\nde".
        let sel = Some((
            Range::new(Position::new(0, 1), Position::new(1, 1)),
            VisualKind::Char,
        ));
        v.render(&buf, &cursor, Mode::Normal, sel, &mut frame, 0, 0, 20, 5);

        let gw = gutter_width(3, true);
        // Line 0: chars 0('a') NOT selected, chars 1-2('b','c') selected,
        // trailing space selected (newline included in multi-line).
        assert!(!is_inverse(&frame, gw, 0)); // 'a'
        assert!(is_inverse(&frame, gw + 1, 0)); // 'b'
        assert!(is_inverse(&frame, gw + 2, 0)); // 'c'
        assert!(is_inverse(&frame, 19, 0)); // trailing (newline)

        // Line 1: chars 0-1('d','e') selected, char 2('f') NOT selected.
        assert!(is_inverse(&frame, gw, 1)); // 'd'
        assert!(is_inverse(&frame, gw + 1, 1)); // 'e'
        assert!(!is_inverse(&frame, gw + 2, 1)); // 'f'

        // Line 2: nothing selected.
        assert!(!is_inverse(&frame, gw, 2));
    }

    #[test]
    fn render_no_selection_no_inverse() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);

        let gw = gutter_width(1, true);
        // No selection: no text cells should be inverse.
        for col in gw..20 {
            assert!(!is_inverse(&frame, col, 0), "col {col} should not be inverse");
        }
    }

    #[test]
    fn render_visual_mode_shows_in_status() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(
            &buf,
            &cursor,
            Mode::Visual(VisualKind::Char),
            None,
            &mut frame,
            0,
            0,
            40,
            3,
        );

        let status = row_chars(&frame, 2);
        assert!(status.contains("VISUAL"), "status = '{status}'");
    }

    #[test]
    fn render_visual_line_mode_shows_in_status() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(40, 3);
        let mut v = View::new();

        v.render(
            &buf,
            &cursor,
            Mode::Visual(VisualKind::Line),
            None,
            &mut frame,
            0,
            0,
            40,
            3,
        );

        let status = row_chars(&frame, 2);
        assert!(status.contains("VISUAL LINE"), "status = '{status}'");
    }

    #[test]
    fn sel_cell_helper() {
        let normal = sel_cell('a', false);
        assert_eq!(normal.character(), Some('a'));
        assert!(!normal.attrs.contains(Attr::INVERSE));

        let selected = sel_cell('b', true);
        assert_eq!(selected.character(), Some('b'));
        assert!(selected.attrs.contains(Attr::INVERSE));
    }

    // ── highlight_matches ───────────────────────────────────────────────

    fn is_yellow_bg(frame: &FrameBuffer, x: u16, y: u16) -> bool {
        frame
            .get(x, y)
            .is_some_and(|c| c.bg == CellColor::Ansi256(3))
    }

    #[test]
    fn highlight_basic() {
        let buf = Buffer::from_text("hello world hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(30, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 30, 3);
        highlight_matches(&v, &mut frame, &buf, "hello", 0, 0, 30, 3);

        let gw = gutter_width(1, true);
        // First "hello" at cols gw..gw+5 should be highlighted.
        for i in 0..5 {
            assert!(is_yellow_bg(&frame, gw + i, 0), "col {} not highlighted", gw + i);
        }
        // Space after first "hello" should NOT be highlighted.
        assert!(!is_yellow_bg(&frame, gw + 5, 0));
        // Second "hello" at cols gw+12..gw+17 should be highlighted.
        for i in 12..17 {
            assert!(is_yellow_bg(&frame, gw + i, 0), "col {} not highlighted", gw + i);
        }
    }

    #[test]
    fn highlight_multi_line() {
        let buf = Buffer::from_text("abc\nabc\nxyz");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 5);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 5);
        highlight_matches(&v, &mut frame, &buf, "abc", 0, 0, 20, 5);

        let gw = gutter_width(3, true);
        // Lines 0 and 1 have "abc" highlighted.
        assert!(is_yellow_bg(&frame, gw, 0));
        assert!(is_yellow_bg(&frame, gw + 2, 0));
        assert!(is_yellow_bg(&frame, gw, 1));
        assert!(is_yellow_bg(&frame, gw + 2, 1));
        // Line 2 "xyz" should NOT be highlighted.
        assert!(!is_yellow_bg(&frame, gw, 2));
    }

    #[test]
    fn highlight_empty_pattern_is_noop() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);
        highlight_matches(&v, &mut frame, &buf, "", 0, 0, 20, 3);

        let gw = gutter_width(1, true);
        // Nothing should be highlighted.
        assert!(!is_yellow_bg(&frame, gw, 0));
    }

    #[test]
    fn highlight_preserves_character() {
        let buf = Buffer::from_text("hello");
        let cursor = Cursor::new();
        let mut frame = FrameBuffer::new(20, 3);
        let mut v = View::new();

        v.render(&buf, &cursor, Mode::Normal, None, &mut frame, 0, 0, 20, 3);
        highlight_matches(&v, &mut frame, &buf, "ell", 0, 0, 20, 3);

        let gw = gutter_width(1, true);
        // Characters should be preserved even though colors changed.
        assert_eq!(frame.get(gw + 1, 0).unwrap().character(), Some('e'));
        assert_eq!(frame.get(gw + 2, 0).unwrap().character(), Some('l'));
        assert_eq!(frame.get(gw + 3, 0).unwrap().character(), Some('l'));
    }

    // ── render_search_line ──────────────────────────────────────────────

    #[test]
    fn search_line_forward() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_search_line(&mut frame, '/', "hello", 5, 0, 0, 20);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with("/hello"), "row = '{row}'");
        // Cursor after input: col 6 (/ + hello)
        assert_eq!(pos, Some((6, 0)));
    }

    #[test]
    fn search_line_backward() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_search_line(&mut frame, '?', "world", 5, 0, 0, 20);

        let row = row_chars(&frame, 0);
        assert!(row.starts_with("?world"), "row = '{row}'");
        assert_eq!(pos, Some((6, 0)));
    }

    #[test]
    fn search_line_empty_input() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_search_line(&mut frame, '/', "", 0, 0, 0, 20);

        assert_eq!(pos, Some((1, 0)));
    }

    #[test]
    fn search_line_zero_width() {
        let mut frame = FrameBuffer::new(20, 1);
        let pos = render_search_line(&mut frame, '/', "hello", 0, 0, 0, 0);
        assert!(pos.is_none());
    }

    #[test]
    fn search_line_fills_remaining() {
        let mut frame = FrameBuffer::new(10, 1);
        for col in 0..10u16 {
            frame.set(col, 0, Cell::new('X'));
        }

        render_search_line(&mut frame, '/', "ab", 2, 0, 0, 10);

        let row = frame.row(0).unwrap();
        assert_eq!(row[0].character(), Some('/'));
        assert_eq!(row[1].character(), Some('a'));
        assert_eq!(row[2].character(), Some('b'));
        for col in 3..10 {
            assert_eq!(row[col].character(), Some(' '), "col {col} not cleared");
        }
    }
}
