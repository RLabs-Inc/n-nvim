// SPDX-License-Identifier: MIT
//
// Differential renderer — the core of frame rendering performance.
//
// Instead of redrawing the entire screen every frame, we compare the current
// FrameBuffer against the previous one and emit ANSI escape sequences only
// for cells that actually changed. In a typical editor session, maybe 1-3
// rows change per keystroke out of 24+ visible rows. Differential rendering
// turns a full-screen repaint into a surgical update.
//
// The pipeline per frame:
//
//   1. Editor paints UI elements to a FrameBuffer (the "current" frame).
//   2. DiffRenderer.render() compares current against the stored previous frame.
//   3. Changed cells are passed to CellWriter, which emits minimal ANSI escapes
//      (skipping redundant cursor moves, colors, and attributes).
//   4. All output is accumulated in OutputBuffer — zero writes to the terminal.
//   5. DiffRenderer.flush() issues a single write() syscall to the terminal.
//
// Optimizations:
//
//   - Row-level skip: entire unchanged rows are detected with a single slice
//     comparison and skipped without iterating individual cells.
//   - Cell equality uses our derived PartialEq on the 16-byte Cell struct.
//   - Synchronized output (DEC 2026) wraps the frame to prevent flicker.
//   - Zero allocation in steady state: the previous-frame buffer is reused
//     via copy_from() — only the first render or a resize allocates.

use std::io::{self, Write};

use crate::ansi;
use crate::buffer::FrameBuffer;
use crate::output::{CellWriter, OutputBuffer};

// ─── RenderStats ─────────────────────────────────────────────────────────────

/// Statistics from a render pass, for profiling and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// Cells that differed from the previous frame and were rendered.
    pub cells_rendered: usize,
    /// Cells that matched the previous frame and were skipped.
    pub cells_skipped: usize,
    /// Total bytes of ANSI output generated.
    pub bytes_written: usize,
}

impl RenderStats {
    /// Total cells processed (rendered + skipped).
    #[inline]
    #[must_use]
    pub const fn total_cells(&self) -> usize {
        self.cells_rendered + self.cells_skipped
    }
}

// ─── DiffRenderer ────────────────────────────────────────────────────────────

/// Differential renderer that emits ANSI only for changed cells.
///
/// Maintains the previous frame for comparison and uses a [`CellWriter`]
/// for stateful output minimization. All output is buffered for a single
/// `write()` syscall per frame.
///
/// # Usage
///
/// ```no_run
/// use n_term::buffer::FrameBuffer;
/// use n_term::diff::DiffRenderer;
///
/// let mut renderer = DiffRenderer::new();
/// let frame = FrameBuffer::new(80, 24);
///
/// // Paint your UI into `frame`...
///
/// let stats = renderer.render(&frame);
/// renderer.flush().unwrap();
/// // stats.cells_rendered tells you how much work was done.
/// ```
pub struct DiffRenderer {
    output: OutputBuffer,
    writer: CellWriter,
    previous: Option<FrameBuffer>,
}

impl DiffRenderer {
    /// Create a renderer with no previous frame (first render will draw everything).
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: OutputBuffer::new(),
            writer: CellWriter::new(),
            previous: None,
        }
    }

    /// Diff the current frame against the previous and generate ANSI output.
    ///
    /// After calling this, use [`flush`](Self::flush) or
    /// [`flush_to`](Self::flush_to) to write the output to the terminal,
    /// or [`output_bytes`](Self::output_bytes) to inspect it (for tests).
    ///
    /// # Panics
    ///
    /// Panics only on internal logic errors (unwrap on in-bounds cell access).
    pub fn render(&mut self, current: &FrameBuffer) -> RenderStats {
        self.output.clear();
        self.writer.reset_state();

        let width = current.width();
        let height = current.height();
        let mut stats = RenderStats::default();

        // Nothing to render for zero-size buffers.
        if width == 0 || height == 0 {
            self.store_frame(current);
            return stats;
        }

        // Synchronized output: terminal buffers until end_sync.
        ansi::begin_sync(&mut self.output).ok();

        // Determine if we need a full redraw (first render or size changed).
        let size_matches = self
            .previous
            .as_ref()
            .is_some_and(|prev| prev.width() == width && prev.height() == height);
        let full_redraw = self.previous.is_none() || !size_matches;

        if full_redraw {
            ansi::clear_screen(&mut self.output).ok();
            ansi::cursor_to(&mut self.output, 0, 0).ok();
        }

        // ── Diff loop ──
        for y in 0..height {
            // Row-skip optimization: if the entire row is unchanged, skip it.
            if !full_redraw {
                if let Some(prev) = &self.previous {
                    if let (Some(curr_row), Some(prev_row)) = (current.row(y), prev.row(y)) {
                        if curr_row == prev_row {
                            stats.cells_skipped += usize::from(width);
                            continue;
                        }
                    }
                }
            }

            for x in 0..width {
                // Safety: x < width and y < height, so unwrap is safe.
                let cell = current.get(x, y).unwrap();

                let changed = full_redraw
                    || self
                        .previous
                        .as_ref()
                        .and_then(|p| p.get(x, y))
                        != Some(cell);

                if changed {
                    self.writer.render_cell(&mut self.output, x, y, cell);
                    stats.cells_rendered += 1;
                } else {
                    stats.cells_skipped += 1;
                }
            }
        }

        // Reset terminal state at frame end to prevent leaking into the
        // terminal's default rendering (cursor line, shell prompt, etc.).
        ansi::reset(&mut self.output).ok();

        ansi::end_sync(&mut self.output).ok();

        stats.bytes_written = self.output.len();

        // Store current frame for next diff (zero allocation in steady state).
        self.store_frame(current);

        stats
    }

    /// The raw ANSI bytes from the last render (for testing and debugging).
    #[must_use]
    pub fn output_bytes(&self) -> &[u8] {
        self.output.as_bytes()
    }

    /// Write accumulated output to stdout and clear the buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to stdout fails.
    pub fn flush(&mut self) -> io::Result<()> {
        self.output.flush_stdout()
    }

    /// Write accumulated output to an arbitrary writer and clear the buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to `w` fails.
    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        self.output.flush_to(w)
    }

    /// Discard the previous frame so the next render draws everything.
    ///
    /// Useful after switching to/from alternate screen, or when the user
    /// requests a manual refresh (Ctrl-L).
    pub fn force_redraw(&mut self) {
        self.previous = None;
    }

    /// Store the current frame for next render's comparison.
    ///
    /// Reuses the existing allocation when dimensions match (zero alloc
    /// in steady state). Only allocates on first render or resize.
    fn store_frame(&mut self, current: &FrameBuffer) {
        match &mut self.previous {
            Some(prev)
                if prev.width() == current.width() && prev.height() == current.height() =>
            {
                prev.copy_from(current);
            }
            _ => {
                self.previous = Some(current.clone());
            }
        }
    }
}

impl Default for DiffRenderer {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Attr, Cell, UnderlineStyle};
    use crate::color::CellColor;

    /// Helper: render a frame and return (stats, output_string).
    fn render_frame(renderer: &mut DiffRenderer, frame: &FrameBuffer) -> (RenderStats, String) {
        let stats = renderer.render(frame);
        let output = String::from_utf8(renderer.output_bytes().to_vec()).unwrap();
        (stats, output)
    }

    // ── First Render ────────────────────────────────────────────────────

    #[test]
    fn first_render_draws_all_cells() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        let (stats, _) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 50);
        assert_eq!(stats.cells_skipped, 0);
        assert_eq!(stats.total_cells(), 50);
    }

    #[test]
    fn first_render_clears_screen() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        let (_, output) = render_frame(&mut renderer, &frame);

        assert!(output.contains("\x1b[2J")); // clear screen
    }

    #[test]
    fn first_render_has_sync_markers() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        let (_, output) = render_frame(&mut renderer, &frame);

        assert!(output.starts_with("\x1b[?2026h")); // begin sync
        assert!(output.ends_with("\x1b[?2026l")); // end sync
    }

    #[test]
    fn first_render_ends_with_reset() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        let (_, output) = render_frame(&mut renderer, &frame);

        // Reset should be just before end_sync.
        assert!(output.contains("\x1b[0m\x1b[?2026l"));
    }

    // ── Identical Frames ────────────────────────────────────────────────

    #[test]
    fn identical_frames_skip_all_cells() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        // First render: draws everything.
        renderer.render(&frame);

        // Second render: nothing changed.
        let (stats, _) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 0);
        assert_eq!(stats.cells_skipped, 50);
    }

    #[test]
    fn identical_frames_no_clear_screen() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);
        let (_, output) = render_frame(&mut renderer, &frame);

        assert!(!output.contains("\x1b[2J")); // no clear screen
    }

    #[test]
    fn identical_frames_minimal_output() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);
        let (stats, _) = render_frame(&mut renderer, &frame);

        // Only sync markers + reset. No cell data.
        // begin_sync(10) + reset(4) + end_sync(10) = 24 bytes.
        assert!(stats.bytes_written < 30);
    }

    // ── Single Cell Change ──────────────────────────────────────────────

    #[test]
    fn single_cell_change_renders_one() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);

        // Change one cell.
        frame.set(3, 2, Cell::new('X'));

        let (stats, output) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 1);
        assert_eq!(stats.cells_skipped, 49);
        assert!(output.contains('X'));
    }

    #[test]
    fn single_cell_change_positions_cursor() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);
        frame.set(7, 4, Cell::new('Z'));

        let (_, output) = render_frame(&mut renderer, &frame);

        // Cursor should move to (7, 4) → ANSI (8, 5).
        assert!(output.contains("\x1b[5;8H"));
    }

    // ── Multiple Changes ────────────────────────────────────────────────

    #[test]
    fn scattered_changes_render_only_changed() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(20, 10);

        renderer.render(&frame);

        // Change 3 cells in different locations.
        frame.set(0, 0, Cell::new('A'));
        frame.set(10, 5, Cell::new('B'));
        frame.set(19, 9, Cell::new('C'));

        let (stats, output) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 3);
        assert_eq!(stats.cells_skipped, 197);
        assert!(output.contains('A'));
        assert!(output.contains('B'));
        assert!(output.contains('C'));
    }

    #[test]
    fn full_row_change_renders_row() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);

        // Change every cell in row 2.
        for x in 0..10 {
            frame.set(x, 2, Cell::new('='));
        }

        let (stats, _) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 10);
        assert_eq!(stats.cells_skipped, 40);
    }

    // ── Resize ──────────────────────────────────────────────────────────

    #[test]
    fn resize_triggers_full_redraw() {
        let mut renderer = DiffRenderer::new();
        let small = FrameBuffer::new(10, 5);
        let big = FrameBuffer::new(20, 10);

        renderer.render(&small);

        let (stats, output) = render_frame(&mut renderer, &big);

        // All cells rendered (size mismatch = full redraw).
        assert_eq!(stats.cells_rendered, 200);
        assert_eq!(stats.cells_skipped, 0);
        assert!(output.contains("\x1b[2J")); // clear screen on resize
    }

    // ── Styled Cells ────────────────────────────────────────────────────

    #[test]
    fn styled_cell_emits_escapes() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 1);

        renderer.render(&frame);

        let cell = Cell::styled(
            'E',
            CellColor::Rgb(255, 0, 0),
            CellColor::Rgb(0, 0, 255),
            Attr::BOLD | Attr::ITALIC,
            UnderlineStyle::Curly,
        );
        frame.set(0, 0, cell);

        let (_, output) = render_frame(&mut renderer, &frame);

        assert!(output.contains("\x1b[1;3m")); // bold + italic
        assert!(output.contains("\x1b[4:3m")); // curly underline
        assert!(output.contains("\x1b[38;2;255;0;0m")); // red fg
        assert!(output.contains("\x1b[48;2;0;0;255m")); // blue bg
        assert!(output.contains('E'));
    }

    // ── Force Redraw ────────────────────────────────────────────────────

    #[test]
    fn force_redraw_renders_everything() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);

        // Without force: nothing to render.
        let (stats, _) = render_frame(&mut renderer, &frame);
        assert_eq!(stats.cells_rendered, 0);

        // Force redraw.
        renderer.force_redraw();

        let (stats, output) = render_frame(&mut renderer, &frame);
        assert_eq!(stats.cells_rendered, 50);
        assert!(output.contains("\x1b[2J")); // clear screen
    }

    // ── Zero-Size Buffer ────────────────────────────────────────────────

    #[test]
    fn zero_size_buffer_produces_no_output() {
        let mut renderer = DiffRenderer::new();
        let frame = FrameBuffer::new(0, 0);

        let (stats, _) = render_frame(&mut renderer, &frame);

        assert_eq!(stats.cells_rendered, 0);
        assert_eq!(stats.cells_skipped, 0);
        assert_eq!(stats.bytes_written, 0);
    }

    // ── Render Stats ────────────────────────────────────────────────────

    #[test]
    fn render_stats_total_cells() {
        let stats = RenderStats {
            cells_rendered: 10,
            cells_skipped: 40,
            bytes_written: 256,
        };
        assert_eq!(stats.total_cells(), 50);
    }

    #[test]
    fn bytes_written_nonzero_on_change() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 5);

        renderer.render(&frame);
        frame.set(0, 0, Cell::new('X'));

        let (stats, _) = render_frame(&mut renderer, &frame);

        assert!(stats.bytes_written > 0);
    }

    // ── Row-Skip Optimization ───────────────────────────────────────────

    #[test]
    fn unchanged_rows_skipped_efficiently() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(100, 50);

        renderer.render(&frame);

        // Change only row 25.
        for x in 0..100 {
            frame.set(x, 25, Cell::new('#'));
        }

        let (stats, _) = render_frame(&mut renderer, &frame);

        // Only row 25 (100 cells) should be rendered.
        assert_eq!(stats.cells_rendered, 100);
        assert_eq!(stats.cells_skipped, 4900);
    }

    // ── Store Frame (steady-state allocation) ───────────────────────────

    #[test]
    fn consecutive_renders_work() {
        let mut renderer = DiffRenderer::new();
        let mut frame = FrameBuffer::new(10, 5);

        // Render 1: initial.
        let (s1, _) = render_frame(&mut renderer, &frame);
        assert_eq!(s1.cells_rendered, 50);

        // Render 2: no change.
        let (s2, _) = render_frame(&mut renderer, &frame);
        assert_eq!(s2.cells_rendered, 0);

        // Render 3: one change.
        frame.set(0, 0, Cell::new('!'));
        let (s3, _) = render_frame(&mut renderer, &frame);
        assert_eq!(s3.cells_rendered, 1);

        // Render 4: revert.
        frame.set(0, 0, Cell::EMPTY);
        let (s4, _) = render_frame(&mut renderer, &frame);
        assert_eq!(s4.cells_rendered, 1);

        // Render 5: no change again.
        let (s5, _) = render_frame(&mut renderer, &frame);
        assert_eq!(s5.cells_rendered, 0);
    }
}
