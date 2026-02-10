// SPDX-License-Identifier: MIT
#![allow(unsafe_code)]
//
// Background stdin reader — collects raw bytes from the terminal.
//
// A dedicated thread reads stdin in blocking mode and sends byte chunks
// through a standard channel. The main thread receives these chunks and
// feeds them to the input parser.
//
// Why a dedicated thread? Because `read()` on stdin blocks, and the
// event loop must remain responsive for rendering, resize handling,
// and escape sequence timeouts. A background reader lets the main loop
// use `recv_timeout()` on the channel for the hybrid event/tick model.
//
// Shutdown: the reader thread uses `poll()` with a short timeout on
// stdin's file descriptor, checking an `AtomicBool` stop flag between
// polls. This lets us shut down cleanly without leaving the thread
// stuck in a blocking `read()`.

#[cfg(unix)]
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Byte chunk read from stdin.
///
/// Sized for typical terminal input: a single keypress is 1-6 bytes,
/// a paste can be kilobytes. 4 KB handles both without waste.
const READ_BUF_SIZE: usize = 4096;

/// How often the reader thread checks the stop flag (milliseconds).
///
/// The thread polls stdin with this timeout, then checks if it should
/// stop. 50ms means shutdown latency is at most 50ms — imperceptible.
const POLL_TIMEOUT_MS: i32 = 50;

/// Background stdin reader thread.
///
/// Spawns a thread that reads raw bytes from stdin and sends them
/// through a channel. The thread runs until [`stop`](Self::stop) is
/// called (or the `StdinReader` is dropped).
///
/// # Example
///
/// ```no_run
/// use n_term::reader::StdinReader;
///
/// let (reader, rx) = StdinReader::spawn();
///
/// // Receive byte chunks from stdin:
/// while let Ok(bytes) = rx.recv() {
///     println!("got {} bytes", bytes.len());
/// }
/// // Reader stops when dropped.
/// ```
pub struct StdinReader {
    /// The reader thread handle. `None` after `stop()` joins it.
    handle: Option<JoinHandle<()>>,
    /// Shared flag to signal the thread to exit.
    stop: Arc<AtomicBool>,
}

impl StdinReader {
    /// Spawn the background reader thread.
    ///
    /// Returns the reader handle and a channel receiver for byte chunks.
    /// Each received `Vec<u8>` is a non-empty chunk of raw stdin data.
    /// The channel closes when the reader is stopped or stdin hits EOF.
    ///
    /// # Panics
    ///
    /// Panics if the OS cannot spawn a new thread (extremely rare).
    #[must_use]
    pub fn spawn() -> (Self, Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("stdin-reader".into())
            .spawn(move || {
                Self::reader_loop(tx, stop_flag);
            })
            .expect("failed to spawn stdin reader thread");

        (
            Self {
                handle: Some(handle),
                stop,
            },
            rx,
        )
    }

    /// Signal the reader thread to stop and wait for it to exit.
    ///
    /// Idempotent: calling `stop()` after the thread has already
    /// exited is a no-op.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// The reader thread's main loop.
    ///
    /// Polls stdin with a short timeout, reads available bytes, and
    /// sends them through the channel. Exits when the stop flag is
    /// set, stdin reaches EOF, or the channel is disconnected.
    #[cfg(unix)]
    #[allow(clippy::needless_pass_by_value)] // Owned values moved into thread closure.
    fn reader_loop(tx: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>) {
        use std::os::unix::io::AsRawFd;

        let stdin_fd = io::stdin().as_raw_fd();
        let mut buf = [0u8; READ_BUF_SIZE];

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Poll stdin for readability with a timeout.
            let ready = unsafe {
                let mut pfd = libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                libc::poll(&raw mut pfd, 1, POLL_TIMEOUT_MS)
            };

            // Timeout or error: loop back to check stop flag.
            if ready <= 0 {
                continue;
            }

            // Data available — read it.
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr().cast(), buf.len()) };

            if n <= 0 {
                // EOF or error — exit the thread.
                break;
            }

            #[allow(clippy::cast_sign_loss)] // n > 0 guaranteed above.
            let chunk = buf[..n as usize].to_vec();

            if tx.send(chunk).is_err() {
                // Receiver dropped — nobody's listening.
                break;
            }
        }
    }

    /// Non-unix fallback using blocking reads with no poll.
    ///
    /// Less graceful shutdown (thread blocks in read), but functional.
    #[cfg(not(unix))]
    #[allow(clippy::needless_pass_by_value)]
    fn reader_loop(tx: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>) {
        use std::io::Read;

        let stdin = std::io::stdin();
        let mut buf = [0u8; READ_BUF_SIZE];

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            match stdin.lock().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
}

impl Drop for StdinReader {
    fn drop(&mut self) {
        self.stop();
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn read_buf_size_reasonable() {
        assert!(READ_BUF_SIZE >= 1024);
        assert!(READ_BUF_SIZE <= 65536);
    }

    #[test]
    fn poll_timeout_reasonable() {
        assert!(POLL_TIMEOUT_MS >= 10);
        assert!(POLL_TIMEOUT_MS <= 500);
    }

    #[test]
    fn spawn_and_stop() {
        // Spawn reader — it won't read anything useful in tests (stdin
        // is not a terminal), but it must not panic or hang.
        let (mut reader, _rx) = StdinReader::spawn();
        reader.stop();
    }

    #[test]
    fn stop_is_idempotent() {
        let (mut reader, _rx) = StdinReader::spawn();
        reader.stop();
        reader.stop(); // Second call must not panic.
    }

    #[test]
    fn drop_stops_reader() {
        let (reader, _rx) = StdinReader::spawn();
        drop(reader); // Must not hang.
    }

    #[test]
    fn channel_closes_on_stop() {
        let (mut reader, rx) = StdinReader::spawn();
        reader.stop();

        // After stop, the channel should be closed — recv should fail.
        // Drain any bytes that arrived before stop.
        while rx.try_recv().is_ok() {}
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn stop_flag_is_atomic() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);

        assert!(!flag.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed));
    }
}
