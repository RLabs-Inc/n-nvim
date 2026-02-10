// SPDX-License-Identifier: MIT
//
// n-term demo — a live event viewer that proves every module works together.
//
// This wires the complete pipeline: Terminal → StdinReader → Parser →
// Events → FrameBuffer → DiffRenderer → single write(). Run it and
// press keys, move the mouse, resize the terminal. Ctrl-Q to quit.
//
// Usage:
//   cargo run -p n-term --example demo
#![allow(clippy::similar_names)] // header_fg/header_bg, status_fg/status_bg are clear.

use std::collections::VecDeque;
use std::time::Instant;

use n_term::buffer::FrameBuffer;
use n_term::cell::{Attr, Cell, UnderlineStyle};
use n_term::color::CellColor;
use n_term::event_loop::{Action, App, EventLoop};
use n_term::input::{Event, KeyCode, KeyEvent, Modifiers};
use n_term::terminal::Size;

/// Maximum number of events to display in the scrolling log.
const MAX_LOG_ENTRIES: usize = 100;

/// No underline — used everywhere we create styled cells without underline.
const NO_UL: UnderlineStyle = UnderlineStyle::None;

/// The demo application state.
struct Demo {
    /// Terminal size (updated on resize).
    size: Size,
    /// Rolling log of event descriptions.
    log: VecDeque<String>,
    /// Total events received.
    event_count: u64,
    /// When the demo started (for uptime display).
    start: Instant,
    /// Cursor blink state (toggles on tick).
    cursor_visible: bool,
    /// Ticks since last cursor blink toggle.
    blink_ticks: u32,
}

impl Demo {
    fn new(size: Size) -> Self {
        Self {
            size,
            log: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            event_count: 0,
            start: Instant::now(),
            cursor_visible: true,
            blink_ticks: 0,
        }
    }

    /// Push an event description to the log.
    fn push_log(&mut self, msg: String) {
        if self.log.len() >= MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        self.log.push_back(msg);
    }

    /// Format an event for display.
    fn format_event(event: &Event) -> String {
        match event {
            Event::Key(ke) => {
                let mods = format_modifiers(ke.modifiers);
                let key = format_keycode(ke.code);
                let kind = format!("{:?}", ke.kind);
                if mods.is_empty() {
                    format!("Key: {key} ({kind})")
                } else {
                    format!("Key: {mods}+{key} ({kind})")
                }
            }
            Event::Mouse(me) => {
                format!("Mouse: {:?} at ({}, {})", me.kind, me.x, me.y)
            }
            Event::Paste(text) => {
                let preview: String = text.chars().take(40).collect();
                let suffix = if text.len() > 40 { "..." } else { "" };
                format!("Paste: \"{preview}{suffix}\" ({} bytes)", text.len())
            }
            Event::FocusGained => "Focus: gained".into(),
            Event::FocusLost => "Focus: lost".into(),
        }
    }
}

/// Format modifier flags as a readable string.
fn format_modifiers(mods: Modifiers) -> String {
    let mut parts = Vec::new();
    if mods.contains(Modifiers::CTRL) {
        parts.push("Ctrl");
    }
    if mods.contains(Modifiers::ALT) {
        parts.push("Alt");
    }
    if mods.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }
    if mods.contains(Modifiers::SUPER) {
        parts.push("Super");
    }
    parts.join("+")
}

/// Format a key code as a readable string.
fn format_keycode(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => format!("'{c}'"),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Escape => "Escape".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::CapsLock => "CapsLock".into(),
        KeyCode::ScrollLock => "ScrollLock".into(),
        KeyCode::NumLock => "NumLock".into(),
        KeyCode::PrintScreen => "PrintScreen".into(),
        KeyCode::Pause => "Pause".into(),
        KeyCode::Menu => "Menu".into(),
    }
}

impl App for Demo {
    fn on_event(&mut self, event: &Event) -> Action {
        self.event_count += 1;

        // Quit on Ctrl-Q or Ctrl-C.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('q' | 'c'),
            modifiers,
            ..
        }) = event
        {
            if modifiers.contains(Modifiers::CTRL) {
                return Action::Quit;
            }
        }

        // Log the event.
        let msg = Self::format_event(event);
        self.push_log(msg);

        Action::Continue
    }

    fn on_resize(&mut self, size: Size) {
        self.size = size;
        self.push_log(format!(
            "Resize: {}x{} ({} cells)",
            size.cols,
            size.rows,
            size.area()
        ));
    }

    fn on_tick(&mut self) -> bool {
        // Cursor blink: toggle every ~60 ticks (500ms at 120fps).
        self.blink_ticks += 1;
        if self.blink_ticks >= 60 {
            self.blink_ticks = 0;
            self.cursor_visible = !self.cursor_visible;
            return true;
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    fn paint(&self, buf: &mut FrameBuffer) {
        let w = buf.width();
        let h = buf.height();

        if w < 20 || h < 5 {
            return; // Too small to draw anything useful.
        }

        // ── Header ───────────────────────────────────────────────
        let header_fg = CellColor::Rgb(0, 0, 0);
        let header_bg = CellColor::Rgb(100, 200, 255);

        for x in 0..w {
            buf.set(x, 0, Cell::styled(' ', header_fg, header_bg, Attr::empty(), NO_UL));
        }

        let title = format!(
            " n-term demo | {}x{} | {} events | {:.1}s ",
            self.size.cols,
            self.size.rows,
            self.event_count,
            self.start.elapsed().as_secs_f64()
        );
        paint_str(buf, 0, 0, &title, header_fg, header_bg, Attr::BOLD);

        // ── Quit hint ────────────────────────────────────────────
        let hint = "Ctrl-Q to quit";
        #[allow(clippy::cast_possible_truncation)] // hint.len() is 14, always fits u16.
        let hint_start = w.saturating_sub(hint.len() as u16 + 1);
        paint_str(buf, hint_start, 0, hint, header_fg, header_bg, Attr::empty());

        // ── Separator ────────────────────────────────────────────
        let sep_fg = CellColor::Rgb(60, 60, 60);
        for x in 0..w {
            buf.set(x, 1, Cell::styled('\u{2500}', sep_fg, CellColor::Default, Attr::empty(), NO_UL));
        }

        // ── Event log ────────────────────────────────────────────
        let log_start_y: u16 = 2;
        let visible_rows = h.saturating_sub(log_start_y + 1);

        let skip = self.log.len().saturating_sub(usize::from(visible_rows));
        for (i, entry) in self.log.iter().skip(skip).enumerate() {
            #[allow(clippy::cast_possible_truncation)] // i < visible_rows which fits u16.
            let y = log_start_y + i as u16;
            if y >= h - 1 {
                break;
            }

            // Alternate row colors for readability.
            let bg = if i % 2 == 0 {
                CellColor::Default
            } else {
                CellColor::Rgb(20, 20, 30)
            };

            // Event type color coding.
            let fg = event_color(entry);

            // Line number.
            let line_num = format!("{:>4} ", self.log.len() - (self.log.len() - skip) + i + 1);
            paint_str(buf, 0, y, &line_num, CellColor::Rgb(80, 80, 80), bg, Attr::empty());

            // Event text.
            let max_len = usize::from(w).saturating_sub(6);
            let truncated: String = entry.chars().take(max_len).collect();
            paint_str(buf, 5, y, &truncated, fg, bg, Attr::empty());

            // Fill rest of line with background.
            #[allow(clippy::cast_possible_truncation)] // truncated.len() < terminal width.
            let text_end = 5 + truncated.len() as u16;
            for x in text_end..w {
                buf.set(x, y, Cell::styled(' ', fg, bg, Attr::empty(), NO_UL));
            }
        }

        // ── Status bar (bottom) ──────────────────────────────────
        paint_status_bar(buf, self.event_count, self.cursor_visible);
    }
}

/// Pick a color based on event type prefix.
fn event_color(entry: &str) -> CellColor {
    if entry.starts_with("Key:") {
        CellColor::Rgb(130, 220, 130) // green
    } else if entry.starts_with("Mouse:") {
        CellColor::Rgb(180, 180, 255) // blue
    } else if entry.starts_with("Paste:") {
        CellColor::Rgb(255, 200, 100) // orange
    } else if entry.starts_with("Focus:") {
        CellColor::Rgb(200, 150, 255) // purple
    } else if entry.starts_with("Resize:") {
        CellColor::Rgb(255, 255, 100) // yellow
    } else {
        CellColor::Rgb(200, 200, 200) // gray
    }
}

/// Paint the bottom status bar.
fn paint_status_bar(buf: &mut FrameBuffer, event_count: u64, cursor_visible: bool) {
    let w = buf.width();
    let h = buf.height();
    let status_y = h - 1;
    let fg = CellColor::Rgb(0, 0, 0);
    let bg = CellColor::Rgb(80, 80, 100);

    for x in 0..w {
        buf.set(x, status_y, Cell::styled(' ', fg, bg, Attr::empty(), NO_UL));
    }

    // Blinking cursor indicator.
    let cursor_char = if cursor_visible { '\u{2588}' } else { ' ' };
    buf.set(1, status_y, Cell::styled(cursor_char, CellColor::Rgb(100, 255, 100), bg, Attr::empty(), NO_UL));

    let status = format!(" {event_count} events | Type, click, scroll, resize — everything is wired");
    paint_str(buf, 3, status_y, &status, fg, bg, Attr::empty());
}

/// Paint a string to the frame buffer at (x, y) with the given style.
fn paint_str(buf: &mut FrameBuffer, x: u16, y: u16, text: &str, fg: CellColor, bg: CellColor, attrs: Attr) {
    let w = buf.width();
    let mut col = x;
    for ch in text.chars() {
        if col >= w {
            break;
        }
        buf.set(col, y, Cell::styled(ch, fg, bg, attrs, NO_UL));
        col += 1;
    }
}

fn main() -> std::io::Result<()> {
    let mut event_loop = EventLoop::new()?;
    let size = event_loop.size();
    let mut app = Demo::new(size);

    // Initial messages showing what protocols are active.
    app.push_log("Welcome to n-term! All modules wired and running.".into());
    app.push_log(format!("Terminal: {}x{} ({} cells)", size.cols, size.rows, size.area()));
    app.push_log("Protocols: SGR mouse, Kitty keyboard, bracketed paste, focus".into());
    app.push_log("Pipeline: stdin \u{2192} Parser \u{2192} Events \u{2192} FrameBuffer \u{2192} DiffRenderer \u{2192} stdout".into());
    app.push_log(String::new());

    event_loop.run(&mut app)?;
    Ok(())
}
