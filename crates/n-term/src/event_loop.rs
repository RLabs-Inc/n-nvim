// SPDX-License-Identifier: MIT
#![allow(unsafe_code)]
//
// Event loop — the heartbeat of the terminal application.
//
// This is the module that wires everything together: stdin bytes flow
// in from the background reader, get parsed into events, the application
// handles them, paints a frame buffer, and the diff renderer outputs
// only what changed to the terminal. One loop. One heartbeat.
//
// # The 120fps Hybrid Model
//
// The loop blocks on the stdin channel with an 8.3ms timeout (120 Hz).
// This gives us three behaviors in one:
//
//   1. **Instant response**: When the user types, bytes arrive on the
//      channel immediately. No polling latency. Sub-millisecond from
//      keypress to rendered frame.
//
//   2. **Zero CPU idle**: When nothing happens, `recv_timeout` blocks
//      the thread. The OS schedules us out. 0% CPU.
//
//   3. **Tick-driven animation**: The timeout fires 120 times per
//      second, giving animations (cursor blink, AI streaming) a
//      consistent tick rate. But we only render if something changed
//      (the dirty flag), so idle screens cost nothing.
//
// # SIGWINCH Handling
//
// Terminal resize is detected via SIGWINCH signal handler that sets an
// `AtomicBool`. The loop checks this flag each iteration and triggers
// a full redraw when the terminal size changes. Maximum latency from
// resize to redraw: 8.3ms (one loop iteration).
//
// # Escape Sequence Timeout
//
// A lone ESC byte is ambiguous: it could be the Escape key or the start
// of a CSI sequence. The parser holds it as "pending." On the next loop
// iteration where no new bytes arrive (timeout fires), we flush pending
// bytes as literal events. With an 8.3ms timeout, the user experiences
// at most 8.3ms lag on Escape — imperceptible.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::ansi;
use crate::buffer::FrameBuffer;
use crate::diff::DiffRenderer;
use crate::input::{Event, Parser};
use crate::reader::StdinReader;
use crate::terminal::{Size, Terminal};

// ─── SIGWINCH ────────────────────────────────────────────────────────────────

/// Global flag set by the SIGWINCH handler. Checked each loop iteration.
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Install a signal handler for SIGWINCH (terminal resize).
///
/// The handler simply sets the [`SIGWINCH_RECEIVED`] flag. This is
/// async-signal-safe: writing to an atomic is one of the few operations
/// permitted inside signal handlers.
#[cfg(unix)]
fn install_sigwinch_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigwinch_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&raw mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &raw const sa, std::ptr::null_mut());
    }
}

#[cfg(unix)]
extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
}

#[cfg(not(unix))]
fn install_sigwinch_handler() {
    // No-op on non-unix platforms.
}

// ─── App Trait ───────────────────────────────────────────────────────────────

/// What the application tells the event loop to do after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Continue running.
    Continue,
    /// Exit the event loop cleanly.
    Quit,
}

/// Application interface for the event loop.
///
/// Implement this trait to create a terminal application. The event loop
/// calls your methods in this order each frame:
///
/// 1. [`on_event`](App::on_event) — for each parsed input event
/// 2. [`on_resize`](App::on_resize) — when the terminal size changes
/// 3. [`on_tick`](App::on_tick) — every loop iteration (for animations)
/// 4. [`paint`](App::paint) — when the frame is dirty and needs redrawing
/// 5. [`cursor`](App::cursor) — after paint, to position the hardware cursor
///
/// Only [`paint`](App::paint) is required. Everything else has default
/// no-op implementations.
pub trait App {
    /// Handle a parsed input event (key, mouse, paste, focus).
    ///
    /// Return [`Action::Quit`] to exit the event loop.
    fn on_event(&mut self, _event: &Event) -> Action {
        Action::Continue
    }

    /// Handle terminal resize.
    ///
    /// Called with the new terminal dimensions. The frame buffer has
    /// already been resized before this is called.
    fn on_resize(&mut self, _size: Size) {}

    /// Called every loop iteration, even when no input arrived.
    ///
    /// Use this for time-based state like cursor blink, animation
    /// progress, or AI streaming indicators. Return `true` if state
    /// changed and a repaint is needed.
    fn on_tick(&mut self) -> bool {
        false
    }

    /// Paint the current application state to the frame buffer.
    ///
    /// Called only when the frame is dirty (input arrived, resize
    /// happened, or `on_tick` returned `true`). The buffer has been
    /// cleared before this call — paint everything you want visible.
    ///
    /// Takes `&mut self` so the application can update render state
    /// (e.g., store the computed cursor screen position for [`cursor`]).
    fn paint(&mut self, buf: &mut FrameBuffer);

    /// The terminal cursor position and shape after painting.
    ///
    /// Return `Some((x, y, shape))` to show the hardware cursor at the
    /// given screen position with the given shape, or `None` to keep the
    /// cursor hidden. Called after every [`paint`] to update the cursor.
    ///
    /// The event loop handles cursor show/hide and shape changes — the
    /// application just reports where the cursor should be.
    fn cursor(&self) -> Option<(u16, u16, crate::ansi::CursorShape)> {
        None
    }
}

// ─── Frame Loop Config ───────────────────────────────────────────────────────

/// Configuration for the event loop timing.
///
/// The defaults are designed for a text editor: 120fps tick rate for
/// smooth cursor blink, and 8.3ms timeout that doubles as the escape
/// sequence timeout.
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    /// Timeout for the channel `recv_timeout` call (microseconds).
    ///
    /// This controls both the tick rate and the escape sequence
    /// timeout. Default: 8333μs (120 Hz).
    pub tick_interval_us: u64,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            tick_interval_us: 8333, // 120 Hz
        }
    }
}

// ─── EventLoop ───────────────────────────────────────────────────────────────

/// The terminal event loop.
///
/// Owns the terminal, parser, renderer, and stdin reader. Call
/// [`run`](Self::run) to enter the loop — it returns when the
/// application signals [`Action::Quit`].
///
/// # Example
///
/// ```no_run
/// use n_term::event_loop::{Action, App, EventLoop};
/// use n_term::buffer::FrameBuffer;
/// use n_term::input::{Event, KeyCode, KeyEvent};
///
/// struct MyApp;
///
/// impl App for MyApp {
///     fn on_event(&mut self, event: &Event) -> Action {
///         if let Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) = event {
///             return Action::Quit;
///         }
///         Action::Continue
///     }
///
///     fn paint(&mut self, buf: &mut FrameBuffer) {
///         // Paint your UI here...
///     }
/// }
///
/// let mut event_loop = EventLoop::new()?;
/// event_loop.run(&mut MyApp)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct EventLoop {
    terminal: Terminal,
    parser: Parser,
    renderer: DiffRenderer,
    config: LoopConfig,
}

impl EventLoop {
    /// Create a new event loop with default configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal cannot be initialized.
    pub fn new() -> io::Result<Self> {
        Self::with_config(LoopConfig::default())
    }

    /// Create a new event loop with custom timing configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal cannot be initialized.
    pub fn with_config(config: LoopConfig) -> io::Result<Self> {
        Ok(Self {
            terminal: Terminal::new()?,
            parser: Parser::new(),
            renderer: DiffRenderer::new(),
            config,
        })
    }

    /// The current terminal size.
    #[inline]
    #[must_use]
    pub const fn size(&self) -> Size {
        self.terminal.size()
    }

    /// Run the event loop until the application returns [`Action::Quit`].
    ///
    /// This method:
    /// 1. Enters TUI mode (raw mode, alternate screen, features)
    /// 2. Installs the SIGWINCH handler
    /// 3. Spawns the background stdin reader
    /// 4. Runs the 120fps hybrid loop
    /// 5. Restores the terminal on exit (even on error)
    ///
    /// # Errors
    ///
    /// Returns an error if terminal enter/leave or rendering fails.
    pub fn run(&mut self, app: &mut impl App) -> io::Result<()> {
        self.terminal.enter()?;
        install_sigwinch_handler();

        let (mut reader, rx) = StdinReader::spawn();

        let result = self.run_inner(app, &rx);

        // Always clean up, even if the loop errored.
        reader.stop();
        self.terminal.leave()?;

        result
    }

    /// The inner loop, separated so cleanup runs regardless of outcome.
    fn run_inner(&mut self, app: &mut impl App, rx: &Receiver<Vec<u8>>) -> io::Result<()> {
        let size = self.terminal.size();
        let mut frame = FrameBuffer::new(size.cols, size.rows);
        let mut dirty = true; // First frame always renders.
        let timeout = Duration::from_micros(self.config.tick_interval_us);

        loop {
            // ── Receive stdin bytes ──────────────────────────────
            match rx.recv_timeout(timeout) {
                Ok(bytes) => {
                    let events = self.parser.advance(&bytes);
                    for event in &events {
                        if app.on_event(event) == Action::Quit {
                            return Ok(());
                        }
                    }
                    if !events.is_empty() {
                        dirty = true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Flush pending escape sequences (lone ESC → Escape key).
                    if self.parser.has_pending() {
                        let events = self.parser.flush();
                        for event in &events {
                            if app.on_event(event) == Action::Quit {
                                return Ok(());
                            }
                        }
                        if !events.is_empty() {
                            dirty = true;
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Reader thread died — exit gracefully.
                    return Ok(());
                }
            }

            // ── Check for terminal resize ────────────────────────
            if SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed) {
                let new_size = self.terminal.refresh_size();
                frame.resize(new_size.cols, new_size.rows);
                self.renderer.force_redraw();
                app.on_resize(new_size);
                dirty = true;
            }

            // ── Tick (animations, time-based state) ──────────────
            if app.on_tick() {
                dirty = true;
            }

            // ── Render if dirty ──────────────────────────────────
            if dirty {
                frame.clear();
                app.paint(&mut frame);
                self.renderer.render(&frame);
                self.renderer.flush()?;

                // Position the hardware cursor after frame output.
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                if let Some((x, y, shape)) = app.cursor() {
                    ansi::cursor_to(&mut lock, x, y)?;
                    ansi::set_cursor_shape(&mut lock, shape)?;
                    ansi::cursor_show(&mut lock)?;
                } else {
                    ansi::cursor_hide(&mut lock)?;
                }
                lock.flush()?;

                dirty = false;
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── LoopConfig ──────────────────────────────────────────────

    #[test]
    fn default_config_is_120fps() {
        let config = LoopConfig::default();
        assert_eq!(config.tick_interval_us, 8333);
    }

    #[test]
    fn custom_config() {
        let config = LoopConfig {
            tick_interval_us: 16667, // 60 Hz
        };
        assert_eq!(config.tick_interval_us, 16667);
    }

    // ── Action ──────────────────────────────────────────────────

    #[test]
    fn action_equality() {
        assert_eq!(Action::Continue, Action::Continue);
        assert_eq!(Action::Quit, Action::Quit);
        assert_ne!(Action::Continue, Action::Quit);
    }

    #[test]
    fn action_debug() {
        let s = format!("{:?}", Action::Continue);
        assert_eq!(s, "Continue");
    }

    // ── EventLoop construction ─────────────────────────────────

    #[test]
    fn event_loop_new_succeeds() {
        let event_loop = EventLoop::new().unwrap();
        let size = event_loop.size();
        assert!(size.cols > 0);
        assert!(size.rows > 0);
    }

    #[test]
    fn event_loop_with_custom_config() {
        let config = LoopConfig {
            tick_interval_us: 16667,
        };
        let event_loop = EventLoop::with_config(config).unwrap();
        assert_eq!(event_loop.config.tick_interval_us, 16667);
    }

    // ── SIGWINCH flag ──────────────────────────────────────────

    #[test]
    fn sigwinch_flag_default_false() {
        // Clear any prior state.
        SIGWINCH_RECEIVED.store(false, Ordering::Relaxed);
        assert!(!SIGWINCH_RECEIVED.load(Ordering::Relaxed));
    }

    #[test]
    fn sigwinch_flag_swap() {
        SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
        let was = SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed);
        assert!(was);
        assert!(!SIGWINCH_RECEIVED.load(Ordering::Relaxed));
    }

    // ── App trait defaults ─────────────────────────────────────

    struct MinimalApp;
    impl App for MinimalApp {
        fn paint(&mut self, _buf: &mut FrameBuffer) {}
    }

    #[test]
    fn app_default_on_event_continues() {
        let mut app = MinimalApp;
        let event = Event::FocusGained;
        assert_eq!(app.on_event(&event), Action::Continue);
    }

    #[test]
    fn app_default_on_tick_not_dirty() {
        let mut app = MinimalApp;
        assert!(!app.on_tick());
    }

    #[test]
    fn app_default_on_resize_is_noop() {
        let mut app = MinimalApp;
        app.on_resize(Size { cols: 100, rows: 50 }); // Must not panic.
    }

    // ── Integration: paint is called with correct buffer size ──

    #[test]
    fn paint_receives_sized_buffer() {
        struct CheckSize;
        impl App for CheckSize {
            fn paint(&mut self, buf: &mut FrameBuffer) {
                assert!(buf.width() > 0);
                assert!(buf.height() > 0);
            }
        }
        let mut app = CheckSize;
        let mut buf = FrameBuffer::new(80, 24);
        app.paint(&mut buf);
    }

    // ── Cursor defaults ───────────────────────────────────────

    #[test]
    fn app_default_cursor_is_none() {
        let app = MinimalApp;
        assert!(app.cursor().is_none());
    }
}
