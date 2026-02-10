// SPDX-License-Identifier: MIT
//
// Terminal control — raw mode, alternate screen, and RAII cleanup.
//
// Safety: This module necessarily uses `unsafe` for termios (tcgetattr,
// tcsetattr), ioctl (TIOCGWINSZ), isatty, and raw fd writes. These are
// the standard POSIX interfaces for terminal control — there is no safe
// alternative. Each unsafe block is minimal and documented.
#![allow(unsafe_code)]
//
// This module owns the terminal's raw state. It enters raw mode via termios,
// switches to the alternate screen, enables modern terminal features (mouse,
// Kitty keyboard, bracketed paste, focus reporting), and guarantees cleanup
// on drop — even if the editor panics mid-frame.
//
// The panic hook deserves special mention: it bypasses Rust's stdout lock
// entirely, writing a pre-built restore sequence directly to fd 1. This
// prevents deadlock if the panic happened while holding the stdout lock
// (common during frame rendering). One raw write, everything restored,
// then the original panic handler prints its message to a working terminal.
//
// Why not crossterm? Same reason we wrote our own ANSI module: a serious
// editor needs direct control over every terminal interaction, not an
// abstraction layer that might make different choices than we would.

use std::io::{self, Write};
use std::sync::{Mutex, Once};

use crate::ansi;

// ─── Size ───────────────────────────────────────────────────────────────────

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Number of columns (width in character cells).
    pub cols: u16,
    /// Number of rows (height in character cells).
    pub rows: u16,
}

impl Size {
    /// Total number of cells (`cols × rows`).
    #[inline]
    #[must_use]
    pub const fn area(self) -> u32 {
        self.cols as u32 * self.rows as u32
    }
}

// ─── Terminal Queries ───────────────────────────────────────────────────────

/// Query the current terminal size via `ioctl(TIOCGWINSZ)`.
///
/// Returns `None` if stdout is not a terminal or the query fails.
#[cfg(unix)]
#[must_use]
pub fn get_size() -> Option<Size> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };

    if result == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(Size {
            cols: ws.ws_col,
            rows: ws.ws_row,
        })
    } else {
        None
    }
}

#[cfg(not(unix))]
#[must_use]
pub fn get_size() -> Option<Size> {
    None
}

/// Check whether stdin is connected to a terminal (TTY).
#[cfg(unix)]
#[must_use]
pub fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

#[cfg(not(unix))]
#[must_use]
pub fn is_tty() -> bool {
    false
}

// ─── Panic-Safe Terminal Restore ────────────────────────────────────────────

/// Global backup of original termios for panic recovery.
///
/// The [`Terminal`] struct owns its own copy, but the panic hook can't
/// access it. This global backup — behind a [`Mutex`], not `static mut` —
/// lets the hook restore raw mode without the struct.
#[cfg(unix)]
static TERMIOS_BACKUP: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Restore termios from the global backup. Best-effort, ignores errors.
#[cfg(unix)]
fn restore_termios_from_backup() {
    if let Ok(guard) = TERMIOS_BACKUP.lock() {
        if let Some(ref original) = *guard {
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
    }
}

/// Complete terminal restore sequence for emergency use.
///
/// Concatenation of: end synchronized output, disable mouse (SGR format +
/// all-motion + drag + click), disable Kitty keyboard, disable bracketed
/// paste, disable focus reporting, reset SGR attributes, reset cursor shape,
/// show cursor, exit alternate screen.
///
/// Ordered carefully: alternate screen exit is last so the restored shell
/// content appears with no TUI artifacts.
#[rustfmt::skip]
const EMERGENCY_RESTORE: &[u8] = b"\
    \x1b[?2026l\
    \x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\
    \x1b[<u\
    \x1b[?2004l\
    \x1b[?1004l\
    \x1b[0m\
    \x1b[0 q\
    \x1b[?25h\
    \x1b[?1049l";

/// Panic hook guard — ensures the hook is installed at most once per process.
static PANIC_HOOK_INSTALLED: Once = Once::new();

/// Install a panic hook that restores the terminal before printing the error.
///
/// Without this, a panic in raw mode leaves the user's terminal broken:
/// no echo, no line editing, no way to read the error message. Our hook
/// writes [`EMERGENCY_RESTORE`] directly to fd 1 (bypassing Rust's stdout
/// lock to avoid deadlock), restores termios, then delegates to the
/// original panic handler so the error prints to a working terminal.
fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();

            #[cfg(unix)]
            restore_termios_from_backup();

            original(info);
        }));
    });
}

/// Write the complete restore sequence directly to stdout's file descriptor.
///
/// Bypasses Rust's `io::stdout()` lock to avoid deadlocking if the panic
/// occurred while the lock was held (e.g., mid-frame flush).
fn emergency_restore() {
    #[cfg(unix)]
    unsafe {
        let _ = libc::write(
            libc::STDOUT_FILENO,
            EMERGENCY_RESTORE.as_ptr().cast::<libc::c_void>(),
            EMERGENCY_RESTORE.len(),
        );
    }

    #[cfg(not(unix))]
    {
        let _ = io::stdout().write_all(EMERGENCY_RESTORE);
        let _ = io::stdout().flush();
    }
}

// ─── Terminal ───────────────────────────────────────────────────────────────

/// Terminal handle with RAII cleanup.
///
/// Call [`enter`](Self::enter) to switch to TUI mode (raw mode, alternate
/// screen, mouse tracking, keyboard protocol). The terminal is automatically
/// restored when the handle is dropped — even on panic.
///
/// # Example
///
/// ```no_run
/// use n_term::terminal::Terminal;
///
/// let mut term = Terminal::new()?;
/// term.enter()?;
/// // ... render frames, handle input ...
/// // Terminal is restored automatically on drop.
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Terminal {
    /// Original termios saved before entering raw mode.
    #[cfg(unix)]
    original_termios: Option<libc::termios>,

    /// Current terminal size (cached, refresh with [`refresh_size`](Self::refresh_size)).
    size: Size,

    /// Whether we're in TUI mode (raw + alt screen + features).
    active: bool,
}

impl Terminal {
    /// Create a terminal handle and query the current size.
    ///
    /// Does **not** enter TUI mode — call [`enter`](Self::enter) for that.
    /// Falls back to 80×24 if the terminal size cannot be determined (e.g.,
    /// in tests or piped environments).
    ///
    /// # Errors
    ///
    /// Currently infallible, but returns `Result` for forward compatibility
    /// (e.g., Windows console API initialization).
    pub fn new() -> io::Result<Self> {
        let size = get_size().unwrap_or(Size { cols: 80, rows: 24 });

        Ok(Self {
            #[cfg(unix)]
            original_termios: None,
            size,
            active: false,
        })
    }

    /// Current terminal size (columns, rows).
    #[inline]
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// Re-query the terminal size from the OS.
    ///
    /// Call this after receiving SIGWINCH to pick up the new dimensions.
    /// Returns the updated size and caches it internally.
    pub fn refresh_size(&mut self) -> Size {
        if let Some(s) = get_size() {
            self.size = s;
        }
        self.size
    }

    /// Whether we're currently in TUI mode.
    #[inline]
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Enter TUI mode.
    ///
    /// Enables raw mode (via termios), switches to the alternate screen,
    /// hides the cursor, clears the screen, and enables:
    /// - SGR mouse tracking (drag mode)
    /// - Kitty keyboard protocol (disambiguate flag)
    /// - Bracketed paste
    /// - Focus reporting
    ///
    /// Idempotent: calling `enter()` while already active is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if raw mode or terminal output fails.
    pub fn enter(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }

        // Install the panic hook (once per process).
        install_panic_hook();

        // Enable raw mode (no-op if not a TTY).
        self.enable_raw_mode()?;

        // Batch all mode-switch sequences to stdout.
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        ansi::enter_alt_screen(&mut lock)?;
        ansi::cursor_hide(&mut lock)?;
        ansi::clear_screen(&mut lock)?;
        ansi::enable_mouse(&mut lock, ansi::MouseMode::Drag)?;
        ansi::enable_kitty_keyboard(&mut lock, 1)?;
        ansi::enable_bracketed_paste(&mut lock)?;
        ansi::enable_focus_reporting(&mut lock)?;
        lock.flush()?;

        self.active = true;
        Ok(())
    }

    /// Leave TUI mode and restore the terminal.
    ///
    /// Disables all features in reverse order, restores the original screen
    /// content, and exits raw mode. Idempotent: calling `leave()` while
    /// inactive is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if terminal output or termios restore fails.
    pub fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let stdout = io::stdout();
        let mut lock = stdout.lock();
        ansi::end_sync(&mut lock)?;
        ansi::disable_focus_reporting(&mut lock)?;
        ansi::disable_bracketed_paste(&mut lock)?;
        ansi::disable_kitty_keyboard(&mut lock)?;
        ansi::disable_mouse(&mut lock)?;
        ansi::reset(&mut lock)?;
        ansi::set_cursor_shape(&mut lock, ansi::CursorShape::Default)?;
        ansi::cursor_show(&mut lock)?;
        ansi::exit_alt_screen(&mut lock)?;
        lock.flush()?;
        drop(lock);

        self.disable_raw_mode()?;
        self.active = false;
        Ok(())
    }

    // ── Raw Mode (termios) ──────────────────────────────────────────

    #[cfg(unix)]
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;

        if !is_tty() {
            return Ok(());
        }

        let fd = io::stdin().as_raw_fd();

        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &raw mut termios) != 0 {
                return Err(io::Error::last_os_error());
            }

            // Save original for restore.
            self.original_termios = Some(termios);

            // Also save to global backup for the panic hook.
            if let Ok(mut guard) = TERMIOS_BACKUP.lock() {
                *guard = Some(termios);
            }

            // cfmakeraw equivalent: disable all line processing.
            termios.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON);
            termios.c_oflag &= !libc::OPOST;
            termios.c_lflag &=
                !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            termios.c_cflag &= !(libc::CSIZE | libc::PARENB);
            termios.c_cflag |= libc::CS8;

            // VMIN=1, VTIME=0: read() blocks until at least 1 byte available.
            termios.c_cc[libc::VMIN] = 1;
            termios.c_cc[libc::VTIME] = 0;

            if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw const termios) != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(())
    }

    #[cfg(not(unix))]
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        if let Some(ref original) = self.original_termios {
            use std::os::unix::io::AsRawFd;
            let fd = io::stdin().as_raw_fd();

            unsafe {
                if libc::tcsetattr(fd, libc::TCSAFLUSH, original) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }

            // Clear the global backup — we've restored successfully.
            if let Ok(mut guard) = TERMIOS_BACKUP.lock() {
                *guard = None;
            }

            self.original_termios = None;
        }

        Ok(())
    }

    #[cfg(not(unix))]
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.active {
            let _ = self.leave();
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Size ──────────────────────────────────────────────────────────

    #[test]
    fn size_area() {
        assert_eq!(Size { cols: 80, rows: 24 }.area(), 1920);
    }

    #[test]
    fn size_area_zero_cols() {
        assert_eq!(Size { cols: 0, rows: 24 }.area(), 0);
    }

    #[test]
    fn size_area_zero_rows() {
        assert_eq!(Size { cols: 80, rows: 0 }.area(), 0);
    }

    #[test]
    fn size_area_large() {
        assert_eq!(Size { cols: 500, rows: 200 }.area(), 100_000);
    }

    #[test]
    fn size_equality() {
        assert_eq!(
            Size { cols: 80, rows: 24 },
            Size { cols: 80, rows: 24 }
        );
    }

    #[test]
    fn size_inequality() {
        assert_ne!(
            Size { cols: 80, rows: 24 },
            Size { cols: 120, rows: 40 }
        );
    }

    #[test]
    fn size_debug_format() {
        let s = Size { cols: 80, rows: 24 };
        let debug = format!("{s:?}");
        assert!(debug.contains("80"));
        assert!(debug.contains("24"));
    }

    #[test]
    fn size_is_copy() {
        let a = Size { cols: 80, rows: 24 };
        let b = a;
        assert_eq!(a, b);
    }

    // ── Terminal queries ─────────────────────────────────────────────

    #[test]
    fn get_size_does_not_panic() {
        let _ = get_size();
    }

    #[test]
    fn is_tty_does_not_panic() {
        let _ = is_tty();
    }

    // ── Emergency restore sequence ──────────────────────────────────

    #[test]
    fn emergency_restore_is_valid_utf8() {
        std::str::from_utf8(EMERGENCY_RESTORE).unwrap();
    }

    #[test]
    fn emergency_restore_exits_alt_screen_last() {
        let s = std::str::from_utf8(EMERGENCY_RESTORE).unwrap();
        assert!(s.ends_with("\x1b[?1049l"));
    }

    #[test]
    fn emergency_restore_contains_all_sequences() {
        let s = std::str::from_utf8(EMERGENCY_RESTORE).unwrap();
        assert!(s.contains("\x1b[?2026l"), "must end sync output");
        assert!(s.contains("\x1b[?1000l"), "must disable mouse clicks");
        assert!(s.contains("\x1b[?1002l"), "must disable mouse drag");
        assert!(s.contains("\x1b[?1003l"), "must disable mouse motion");
        assert!(s.contains("\x1b[?1006l"), "must disable SGR mouse format");
        assert!(s.contains("\x1b[<u"), "must disable kitty keyboard");
        assert!(s.contains("\x1b[?2004l"), "must disable bracketed paste");
        assert!(s.contains("\x1b[?1004l"), "must disable focus reporting");
        assert!(s.contains("\x1b[0m"), "must reset SGR attributes");
        assert!(s.contains("\x1b[0 q"), "must reset cursor shape");
        assert!(s.contains("\x1b[?25h"), "must show cursor");
    }

    // ── Terminal struct ─────────────────────────────────────────────

    #[test]
    fn terminal_new_succeeds() {
        let term = Terminal::new().unwrap();
        assert!(!term.is_active());
    }

    #[test]
    fn terminal_has_reasonable_default_size() {
        let term = Terminal::new().unwrap();
        let s = term.size();
        assert!(s.cols > 0);
        assert!(s.rows > 0);
    }

    #[test]
    fn terminal_enter_leave_cycle() {
        let mut term = Terminal::new().unwrap();
        assert!(!term.is_active());

        term.enter().unwrap();
        assert!(term.is_active());

        term.leave().unwrap();
        assert!(!term.is_active());
    }

    #[test]
    fn terminal_double_enter_is_idempotent() {
        let mut term = Terminal::new().unwrap();
        term.enter().unwrap();
        term.enter().unwrap();
        assert!(term.is_active());
        term.leave().unwrap();
    }

    #[test]
    fn terminal_double_leave_is_idempotent() {
        let mut term = Terminal::new().unwrap();
        term.enter().unwrap();
        term.leave().unwrap();
        term.leave().unwrap();
        assert!(!term.is_active());
    }

    #[test]
    fn terminal_leave_without_enter() {
        let mut term = Terminal::new().unwrap();
        term.leave().unwrap();
        assert!(!term.is_active());
    }

    #[test]
    fn terminal_drop_after_enter() {
        let mut term = Terminal::new().unwrap();
        term.enter().unwrap();
        drop(term);
    }

    #[test]
    fn terminal_drop_without_enter() {
        let term = Terminal::new().unwrap();
        drop(term);
    }

    #[test]
    fn terminal_multiple_cycles() {
        let mut term = Terminal::new().unwrap();
        for _ in 0..3 {
            term.enter().unwrap();
            assert!(term.is_active());
            term.leave().unwrap();
            assert!(!term.is_active());
        }
    }

    #[test]
    fn terminal_refresh_size() {
        let mut term = Terminal::new().unwrap();
        let s = term.refresh_size();
        assert!(s.cols > 0);
        assert!(s.rows > 0);
        assert_eq!(s, term.size());
    }
}
