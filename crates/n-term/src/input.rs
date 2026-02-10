// SPDX-License-Identifier: MIT
//
// Terminal input parser.
//
// Turns raw stdin bytes into structured events: keys, mouse actions,
// paste content, and focus changes. Handles every protocol we enable
// in `terminal.rs`:
//
// - Legacy CSI sequences (arrows, function keys, editing keys)
// - SS3 sequences (F1-F4 alternate encoding from some terminals)
// - SGR mouse protocol (press / release / drag / move / scroll)
// - Kitty keyboard protocol (unambiguous codepoints + modifiers)
// - Bracketed paste (accumulates pasted text between delimiters)
// - Focus reporting (terminal gained / lost focus)
// - Alt+key (ESC followed by printable character)
// - UTF-8 multi-byte characters
//
// # Design
//
// The parser maintains a small internal byte buffer because escape
// sequences can span multiple `read()` calls. Feed bytes with
// [`Parser::advance`], retrieve events from the returned `Vec`.
// After a timeout with no new bytes, call [`Parser::flush`] to
// emit any pending lone ESC as a real Escape keypress.
//
// Number parsing is done directly on `&[u8]` — no intermediate
// `String` allocation for CSI parameter decoding.

use bitflags::bitflags;

// ─── Event Types ────────────────────────────────────────────────────────────

/// A parsed terminal input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A keyboard event (press, repeat, or release).
    Key(KeyEvent),
    /// A mouse event (button action or movement with position).
    Mouse(MouseEvent),
    /// Bracketed paste content.
    ///
    /// The terminal wraps clipboard paste with `CSI 200~` / `CSI 201~`
    /// delimiters. We accumulate the raw bytes between them and deliver
    /// the result as a single event. This lets the editor distinguish
    /// typed input from pasted text (no auto-indent cascade on paste).
    Paste(String),
    /// Terminal window gained focus (`CSI I`).
    FocusGained,
    /// Terminal window lost focus (`CSI O`).
    FocusLost,
}

/// A keyboard event with key identity, modifiers, and press state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Which key was pressed.
    pub code: KeyCode,
    /// Active modifier keys (Shift, Alt, Ctrl, etc.).
    pub modifiers: Modifiers,
    /// Press, repeat, or release (Kitty keyboard protocol).
    pub kind: KeyEventKind,
}

/// Key press / repeat / release distinction.
///
/// With Kitty keyboard protocol flags >= 2, the terminal reports
/// whether a key event is an initial press, an auto-repeat, or a
/// release. Without Kitty protocol (or with flags < 2), all events
/// are reported as [`Press`](KeyEventKind::Press).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyEventKind {
    /// Initial key press (or legacy mode where state is unknown).
    #[default]
    Press,
    /// Key held down long enough to trigger auto-repeat.
    Repeat,
    /// Key released.
    Release,
}

/// Identity of a key.
///
/// Named keys have dedicated variants; printable characters use
/// [`Char`](KeyCode::Char). Function keys F1–F35 use [`F`](KeyCode::F).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A Unicode character (printable).
    Char(char),
    // ── Named keys ──────────────────────────────────────────────
    Enter,
    Tab,
    Backspace,
    Escape,
    Delete,
    Insert,
    // ── Navigation ──────────────────────────────────────────────
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    // ── Function keys ───────────────────────────────────────────
    /// F1 through F35.
    F(u8),
    // ── Lock / modifier keys (Kitty protocol) ───────────────────
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
}

bitflags! {
    /// Keyboard modifier flags.
    ///
    /// Matches the Kitty keyboard protocol bitmask (also compatible
    /// with xterm CSI modifier encoding where `param = 1 + bitmask`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 0b0000_0001;
        const ALT   = 0b0000_0010;
        const CTRL  = 0b0000_0100;
        const SUPER = 0b0000_1000;
        const HYPER = 0b0001_0000;
        const META  = 0b0010_0000;
    }
}

/// A mouse event with button/scroll/move action, position, and modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// What happened (press, release, drag, move, scroll).
    pub kind: MouseEventKind,
    /// 0-indexed column.
    pub x: u16,
    /// 0-indexed row.
    pub y: u16,
    /// Active modifier keys during the mouse event.
    pub modifiers: Modifiers,
}

/// Mouse event classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    /// Button pressed.
    Press(MouseButton),
    /// Button released.
    Release(MouseButton),
    /// Mouse moved while a button is held.
    Drag(MouseButton),
    /// Mouse moved without any button held.
    Move,
    /// Scroll wheel up.
    ScrollUp,
    /// Scroll wheel down.
    ScrollDown,
    /// Scroll wheel left (horizontal scroll, e.g. Shift+ScrollUp on some terminals).
    ScrollLeft,
    /// Scroll wheel right (horizontal scroll).
    ScrollRight,
}

/// Mouse button identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

// ─── Parser ─────────────────────────────────────────────────────────────────

/// Bracketed paste opening delimiter: `ESC [ 200 ~`
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed paste closing delimiter: `ESC [ 201 ~`
const PASTE_END: &[u8] = b"\x1b[201~";

/// Terminal input parser.
///
/// Feed raw bytes via [`advance`](Parser::advance) and collect structured
/// [`Event`]s. The parser buffers incomplete sequences internally and
/// resumes parsing when more bytes arrive.
///
/// # Escape vs escape-sequence ambiguity
///
/// A bare `ESC` byte (0x1B) could be either a standalone Escape keypress
/// or the start of a multi-byte escape sequence. The parser returns
/// `Incomplete` when it sees a lone ESC. The caller should wait a
/// short timeout (~10ms) and then call [`flush`](Parser::flush) to emit
/// the pending ESC as a real Escape key event.
pub struct Parser {
    /// Accumulated raw bytes waiting to be parsed.
    buf: Vec<u8>,
    /// When `true`, we're inside a bracketed paste and accumulating
    /// raw bytes until the closing delimiter arrives.
    in_paste: bool,
}

impl Parser {
    /// Create a new parser with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64),
            in_paste: false,
        }
    }

    /// Feed raw bytes from stdin and return all events that can be parsed.
    ///
    /// Bytes that form an incomplete sequence are kept in the internal
    /// buffer and will be combined with future [`advance`](Parser::advance)
    /// calls. Call [`flush`](Parser::flush) after a timeout to emit any
    /// pending lone ESC.
    pub fn advance(&mut self, data: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(data);
        let mut events = Vec::new();
        let mut pos = 0;

        while pos < self.buf.len() {
            // ── Paste mode: scan for closing delimiter ──────────────
            if self.in_paste {
                let remaining = &self.buf[pos..];
                if let Some(end_offset) = find_subsequence(remaining, PASTE_END) {
                    // Everything before the delimiter is paste content.
                    let text = String::from_utf8_lossy(&remaining[..end_offset]).into_owned();
                    events.push(Event::Paste(text));
                    pos += end_offset + PASTE_END.len();
                    self.in_paste = false;
                } else {
                    // Delimiter not yet found — keep all bytes pending.
                    break;
                }
                continue;
            }

            // ── Paste start: check before general parsing ───────────
            // We detect `CSI 200~` here so `parse_csi` never sees it.
            let remaining = &self.buf[pos..];
            if remaining.len() >= PASTE_START.len()
                && remaining[..PASTE_START.len()] == *PASTE_START
            {
                self.in_paste = true;
                pos += PASTE_START.len();
                continue;
            }
            // If the buffer starts with what *could* become `CSI 200~`
            // but is shorter, and starts with ESC [, check if the
            // partial matches the paste prefix.
            if remaining.len() < PASTE_START.len()
                && PASTE_START.starts_with(remaining)
                && remaining.starts_with(b"\x1b[")
            {
                // Might be a partial paste start — wait for more data.
                // (But could also be a different CSI sequence. We only
                // stall here if the bytes match the paste prefix exactly.)
                break;
            }

            // ── Normal parsing ──────────────────────────────────────
            match try_parse(&self.buf, pos) {
                Parsed::Event(event, consumed) => {
                    events.push(event);
                    pos += consumed;
                }
                Parsed::Incomplete => break,
                Parsed::Skip(n) => pos += n,
            }
        }

        // Compact: remove consumed bytes, keep unconsumed remainder.
        if pos > 0 {
            self.buf.drain(..pos);
        }

        events
    }

    /// Are there unconsumed bytes that might complete with more data?
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Flush pending bytes as literal key events.
    ///
    /// Called after a timeout (typically ~10ms) to resolve the ESC
    /// ambiguity: a lone ESC byte becomes an Escape key event, and
    /// any other leftover bytes become `Char` events.
    pub fn flush(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for &byte in &self.buf {
            let code = match byte {
                0x1B => KeyCode::Escape,
                0x00 => KeyCode::Char('@'),
                b @ 0x01..=0x1A => KeyCode::Char((b + b'a' - 1) as char),
                0x7F => KeyCode::Backspace,
                b @ 0x20..=0x7E => KeyCode::Char(b as char),
                _ => continue,
            };
            let modifiers = match byte {
                0x00..=0x1A => Modifiers::CTRL,
                _ => Modifiers::empty(),
            };
            events.push(Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
            }));
        }
        self.buf.clear();
        events
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Stateless Parsing Functions ────────────────────────────────────────────
//
// All parse functions are pure — they read from `buf[pos..]` and return
// what they found plus how many bytes to consume. No mutable state.

/// Result of trying to parse one event from the buffer.
enum Parsed {
    /// Successfully parsed an event, consuming `usize` bytes.
    Event(Event, usize),
    /// Sequence is incomplete — need more bytes.
    Incomplete,
    /// Unrecognized byte(s), skip `usize` bytes.
    Skip(usize),
}

/// Try to parse a single event starting at `buf[pos]`.
fn try_parse(buf: &[u8], pos: usize) -> Parsed {
    let remaining = &buf[pos..];
    if remaining.is_empty() {
        return Parsed::Skip(0);
    }

    match remaining[0] {
        // ESC — could be escape sequence or standalone Escape key.
        0x1B => parse_escape(remaining),
        // Control characters.
        0x00 => Parsed::Event(ctrl_key(KeyCode::Char('@')), 1),
        b @ (0x01..=0x07 | 0x0B..=0x0C | 0x0E..=0x1A) => Parsed::Event(
            ctrl_key(KeyCode::Char((b + b'a' - 1) as char)),
            1,
        ),
        0x08 | 0x7F => Parsed::Event(press(KeyCode::Backspace), 1),
        0x09 => Parsed::Event(press(KeyCode::Tab), 1),
        0x0A | 0x0D => Parsed::Event(press(KeyCode::Enter), 1),
        // ASCII printable.
        b @ 0x20..=0x7E => Parsed::Event(press(KeyCode::Char(b as char)), 1),
        // UTF-8 multi-byte.
        0xC0..=0xFF => parse_utf8(remaining),
        // Bare continuation bytes (0x80..=0xBF) — invalid lead, skip.
        _ => Parsed::Skip(1),
    }
}

// ── Escape sequences ────────────────────────────────────────────────────────

fn parse_escape(buf: &[u8]) -> Parsed {
    debug_assert_eq!(buf[0], 0x1B);

    if buf.len() < 2 {
        return Parsed::Incomplete;
    }

    match buf[1] {
        // CSI: ESC [
        b'[' => parse_csi(buf),
        // SS3: ESC O
        b'O' => parse_ss3(buf),
        // Alt+ESC.
        0x1B => Parsed::Event(
            Event::Key(KeyEvent {
                code: KeyCode::Escape,
                modifiers: Modifiers::ALT,
                kind: KeyEventKind::Press,
            }),
            2,
        ),
        // Alt+printable character.
        b @ 0x20..=0x7E => Parsed::Event(
            Event::Key(KeyEvent {
                code: KeyCode::Char(b as char),
                modifiers: Modifiers::ALT,
                kind: KeyEventKind::Press,
            }),
            2,
        ),
        // Alt+control character (e.g., ESC Ctrl+A).
        b @ 0x01..=0x1A => Parsed::Event(
            Event::Key(KeyEvent {
                code: KeyCode::Char((b + b'a' - 1) as char),
                modifiers: Modifiers::ALT | Modifiers::CTRL,
                kind: KeyEventKind::Press,
            }),
            2,
        ),
        // Unknown byte after ESC — emit standalone Escape.
        _ => Parsed::Event(press(KeyCode::Escape), 1),
    }
}

// ── CSI (Control Sequence Introducer) ───────────────────────────────────────

fn parse_csi(buf: &[u8]) -> Parsed {
    debug_assert!(buf.len() >= 2 && buf[0] == 0x1B && buf[1] == b'[');

    if buf.len() < 3 {
        return Parsed::Incomplete;
    }

    // SGR mouse: ESC [ <
    if buf[2] == b'<' {
        return parse_sgr_mouse(buf);
    }

    // Focus reporting: ESC [ I (gained) / ESC [ O (lost).
    if buf[2] == b'I' {
        return Parsed::Event(Event::FocusGained, 3);
    }
    if buf[2] == b'O' {
        return Parsed::Event(Event::FocusLost, 3);
    }

    // Scan for the final byte (0x40..=0x7E).
    // CSI parameter bytes are in 0x30..=0x3F, intermediate in 0x20..=0x2F.
    let mut end = 2;
    while end < buf.len() {
        let b = buf[end];
        if (0x40..=0x7E).contains(&b) {
            break;
        }
        if !(0x20..=0x3F).contains(&b) {
            // Invalid byte in CSI sequence — abort.
            return Parsed::Skip(end + 1);
        }
        end += 1;
    }

    if end >= buf.len() {
        return Parsed::Incomplete;
    }

    let final_byte = buf[end];
    let params_raw = &buf[2..end];
    let consumed = end + 1;

    // ── Tilde-terminated sequences (editing keys, function keys) ─────
    if final_byte == b'~' {
        let params = parse_csi_params(params_raw);
        let first = params.first().map_or(0, |p| p.0);
        let modifiers = params
            .get(1)
            .map_or(Modifiers::empty(), |p| decode_modifiers(p.0));

        return match first {
            1 | 7 => Parsed::Event(key_with(KeyCode::Home, modifiers), consumed),
            2 => Parsed::Event(key_with(KeyCode::Insert, modifiers), consumed),
            3 => Parsed::Event(key_with(KeyCode::Delete, modifiers), consumed),
            4 | 8 => Parsed::Event(key_with(KeyCode::End, modifiers), consumed),
            5 => Parsed::Event(key_with(KeyCode::PageUp, modifiers), consumed),
            6 => Parsed::Event(key_with(KeyCode::PageDown, modifiers), consumed),
            15 => Parsed::Event(key_with(KeyCode::F(5), modifiers), consumed),
            17 => Parsed::Event(key_with(KeyCode::F(6), modifiers), consumed),
            18 => Parsed::Event(key_with(KeyCode::F(7), modifiers), consumed),
            19 => Parsed::Event(key_with(KeyCode::F(8), modifiers), consumed),
            20 => Parsed::Event(key_with(KeyCode::F(9), modifiers), consumed),
            21 => Parsed::Event(key_with(KeyCode::F(10), modifiers), consumed),
            23 => Parsed::Event(key_with(KeyCode::F(11), modifiers), consumed),
            24 => Parsed::Event(key_with(KeyCode::F(12), modifiers), consumed),
            25 => Parsed::Event(key_with(KeyCode::F(13), modifiers), consumed),
            26 => Parsed::Event(key_with(KeyCode::F(14), modifiers), consumed),
            28 => Parsed::Event(key_with(KeyCode::F(15), modifiers), consumed),
            29 => Parsed::Event(key_with(KeyCode::F(16), modifiers), consumed),
            31 => Parsed::Event(key_with(KeyCode::F(17), modifiers), consumed),
            32 => Parsed::Event(key_with(KeyCode::F(18), modifiers), consumed),
            33 => Parsed::Event(key_with(KeyCode::F(19), modifiers), consumed),
            34 => Parsed::Event(key_with(KeyCode::F(20), modifiers), consumed),
            _ => Parsed::Skip(consumed),
        };
    }

    // ── Kitty keyboard: CSI codepoint [; modifiers[:event_type]] u ───
    if final_byte == b'u' {
        return parse_kitty_key(params_raw, consumed);
    }

    // ── Standard CSI sequences with letter final bytes ──────────────
    let params = parse_csi_params(params_raw);
    let modifiers = params
        .get(1)
        .map_or(Modifiers::empty(), |p| decode_modifiers(p.0));

    let event = match final_byte {
        b'A' => key_with(KeyCode::Up, modifiers),
        b'B' => key_with(KeyCode::Down, modifiers),
        b'C' => key_with(KeyCode::Right, modifiers),
        b'D' => key_with(KeyCode::Left, modifiers),
        b'H' => key_with(KeyCode::Home, modifiers),
        b'F' => key_with(KeyCode::End, modifiers),
        b'P' => key_with(KeyCode::F(1), modifiers),
        b'Q' => key_with(KeyCode::F(2), modifiers),
        b'R' => key_with(KeyCode::F(3), modifiers),
        b'S' => key_with(KeyCode::F(4), modifiers),
        b'Z' => Event::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: Modifiers::SHIFT,
            kind: KeyEventKind::Press,
        }),
        _ => return Parsed::Skip(consumed),
    };

    Parsed::Event(event, consumed)
}

// ── SS3 (Single Shift 3) ───────────────────────────────────────────────────

fn parse_ss3(buf: &[u8]) -> Parsed {
    debug_assert!(buf.len() >= 2 && buf[0] == 0x1B && buf[1] == b'O');

    if buf.len() < 3 {
        return Parsed::Incomplete;
    }

    let event = match buf[2] {
        b'A' => press(KeyCode::Up),
        b'B' => press(KeyCode::Down),
        b'C' => press(KeyCode::Right),
        b'D' => press(KeyCode::Left),
        b'H' => press(KeyCode::Home),
        b'F' => press(KeyCode::End),
        b'P' => press(KeyCode::F(1)),
        b'Q' => press(KeyCode::F(2)),
        b'R' => press(KeyCode::F(3)),
        b'S' => press(KeyCode::F(4)),
        _ => return Parsed::Skip(3),
    };

    Parsed::Event(event, 3)
}

// ── SGR Mouse Protocol ─────────────────────────────────────────────────────

fn parse_sgr_mouse(buf: &[u8]) -> Parsed {
    // Format: ESC [ < Pb ; Px ; Py M    (press/motion)
    //         ESC [ < Pb ; Px ; Py m    (release)
    debug_assert!(buf.len() >= 3 && buf[2] == b'<');

    // Find terminal byte (M for press/motion, m for release).
    let start = 3;
    let mut end = start;
    while end < buf.len() {
        if buf[end] == b'M' || buf[end] == b'm' {
            break;
        }
        // Valid chars in SGR mouse params: digits and semicolons.
        if !buf[end].is_ascii_digit() && buf[end] != b';' {
            return Parsed::Skip(end + 1);
        }
        end += 1;
    }

    if end >= buf.len() {
        return Parsed::Incomplete;
    }

    let is_release = buf[end] == b'm';
    let consumed = end + 1;

    // Parse three semicolon-separated numbers: button_flags ; x ; y
    let params = &buf[start..end];
    let (cb, rest) = parse_u16_from(params);
    let rest = skip_byte(rest, b';');
    let (raw_x, rest) = parse_u16_from(rest);
    let rest = skip_byte(rest, b';');
    let (raw_y, _) = parse_u16_from(rest);

    // SGR coordinates are 1-indexed; we use 0-indexed.
    let x = raw_x.saturating_sub(1);
    let y = raw_y.saturating_sub(1);

    // Decode modifier flags from button byte.
    let mut modifiers = Modifiers::empty();
    if cb & 4 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if cb & 8 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if cb & 16 != 0 {
        modifiers |= Modifiers::CTRL;
    }

    let is_scroll = cb & 64 != 0;
    let is_motion = cb & 32 != 0;
    let base = cb & 3;

    let kind = if is_scroll {
        // Scroll wheel events. Base 0–3 → Up/Down/Left/Right.
        match base {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if is_motion {
        // Motion event (bit 5 set). If base < 3, a button is held → drag.
        match base {
            0 => MouseEventKind::Drag(MouseButton::Left),
            1 => MouseEventKind::Drag(MouseButton::Middle),
            2 => MouseEventKind::Drag(MouseButton::Right),
            _ => MouseEventKind::Move,
        }
    } else if is_release {
        MouseEventKind::Release(decode_mouse_button(base))
    } else {
        MouseEventKind::Press(decode_mouse_button(base))
    };

    Parsed::Event(
        Event::Mouse(MouseEvent { kind, x, y, modifiers }),
        consumed,
    )
}

// ── Kitty Keyboard Protocol ────────────────────────────────────────────────

fn parse_kitty_key(params_raw: &[u8], consumed: usize) -> Parsed {
    // Format: CSI codepoint [; modifiers[:event_type]] u
    let params = parse_csi_params(params_raw);

    let codepoint = params.first().map_or(0, |p| p.0);

    let (modifier_val, event_type) = params
        .get(1)
        .map_or((0, 0), |p| (p.0, p.1));

    let modifiers = if modifier_val > 0 {
        decode_modifiers(modifier_val)
    } else {
        Modifiers::empty()
    };

    let kind = match event_type {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    };

    let code = kitty_codepoint_to_keycode(codepoint);

    Parsed::Event(
        Event::Key(KeyEvent { code, modifiers, kind }),
        consumed,
    )
}

// ── UTF-8 ──────────────────────────────────────────────────────────────────

fn parse_utf8(buf: &[u8]) -> Parsed {
    let expected = utf8_char_len(buf[0]);

    if expected == 0 {
        return Parsed::Skip(1);
    }
    if buf.len() < expected {
        return Parsed::Incomplete;
    }

    // Validate continuation bytes (must start with 0b10xxxxxx).
    for &b in &buf[1..expected] {
        if b & 0xC0 != 0x80 {
            return Parsed::Skip(1);
        }
    }

    std::str::from_utf8(&buf[..expected]).map_or(Parsed::Skip(1), |s| {
        s.chars()
            .next()
            .map_or(Parsed::Skip(expected), |ch| {
                Parsed::Event(press(KeyCode::Char(ch)), expected)
            })
    })
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Create a simple key press event with no modifiers.
const fn press(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: Modifiers::empty(),
        kind: KeyEventKind::Press,
    })
}

/// Create a Ctrl+key press event.
const fn ctrl_key(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: Modifiers::CTRL,
        kind: KeyEventKind::Press,
    })
}

/// Create a key press event with specific modifiers.
const fn key_with(code: KeyCode, modifiers: Modifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
    })
}

/// CSI parameter: `(main_value, colon_sub_parameter)`.
///
/// The colon sub-parameter is used by the Kitty keyboard protocol
/// to encode event type within the modifier parameter: `modifier:event_type`.
struct CsiParam(u16, u16);

/// Parse semicolon-separated CSI parameters with optional colon sub-params.
///
/// Examples:
/// - `1;2` → `[(1,0), (2,0)]`
/// - `97;5:2` → `[(97,0), (5,2)]`
/// - (empty) → `[]`
fn parse_csi_params(raw: &[u8]) -> Vec<CsiParam> {
    if raw.is_empty() {
        return Vec::new();
    }

    let mut params = Vec::with_capacity(4);
    let mut pos = 0;

    while pos <= raw.len() {
        let (main_val, next) = parse_u16_at(raw, pos);
        pos = next;

        // Check for colon sub-parameter.
        let sub_val = if pos < raw.len() && raw[pos] == b':' {
            pos += 1;
            let (v, n) = parse_u16_at(raw, pos);
            pos = n;
            v
        } else {
            0
        };

        params.push(CsiParam(main_val, sub_val));

        // Skip semicolon separator.
        if pos < raw.len() && raw[pos] == b';' {
            pos += 1;
        } else {
            break;
        }
    }

    params
}

/// Parse a u16 from bytes starting at `start`, stopping at non-digit.
/// Returns `(value, next_position)`.
fn parse_u16_at(buf: &[u8], start: usize) -> (u16, usize) {
    let mut val: u16 = 0;
    let mut pos = start;
    while pos < buf.len() && buf[pos].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add(u16::from(buf[pos] - b'0'));
        pos += 1;
    }
    (val, pos)
}

/// Parse a u16 from the start of a byte slice.
/// Returns `(value, remaining_bytes)`.
fn parse_u16_from(buf: &[u8]) -> (u16, &[u8]) {
    let mut val: u16 = 0;
    let mut pos = 0;
    while pos < buf.len() && buf[pos].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add(u16::from(buf[pos] - b'0'));
        pos += 1;
    }
    (val, &buf[pos..])
}

/// Skip a leading byte if it matches `expected`.
fn skip_byte(buf: &[u8], expected: u8) -> &[u8] {
    if buf.first() == Some(&expected) {
        &buf[1..]
    } else {
        buf
    }
}

/// Decode CSI modifier parameter into `Modifiers` bitflags.
///
/// The encoding is `1 + bitmask`, matching both xterm and Kitty protocols.
/// A parameter of 0 or 1 means no modifiers.
/// The truncation to u8 is intentional: only the low 6 bits carry
/// modifier flags (Shift, Alt, Ctrl, Super, Hyper, Meta).
#[allow(clippy::cast_possible_truncation)]
const fn decode_modifiers(param: u16) -> Modifiers {
    let val = if param > 0 { param - 1 } else { 0 };
    Modifiers::from_bits_truncate(val as u8)
}

/// Map SGR mouse base button value to `MouseButton`.
const fn decode_mouse_button(base: u16) -> MouseButton {
    match base {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        _ => MouseButton::Right,
    }
}

/// Map a Kitty keyboard protocol codepoint to `KeyCode`.
///
/// Standard Unicode codepoints map to `Char`. Functional keys use
/// the Kitty-specific range starting at 57344 (Unicode Private Use Area).
fn kitty_codepoint_to_keycode(cp: u16) -> KeyCode {
    match cp {
        // Escape: ASCII 27 or Kitty PUA 57344.
        27 | 57344 => KeyCode::Escape,
        // Enter: ASCII 13 or Kitty PUA 57345.
        13 | 57345 => KeyCode::Enter,
        // Tab: ASCII 9 or Kitty PUA 57346.
        9 | 57346 => KeyCode::Tab,
        // Backspace: ASCII 127 or Kitty PUA 57347.
        127 | 57347 => KeyCode::Backspace,
        // Kitty functional key codepoints (Unicode PUA).
        57348 => KeyCode::Insert,
        57349 => KeyCode::Delete,
        57350 => KeyCode::Left,
        57351 => KeyCode::Right,
        57352 => KeyCode::Up,
        57353 => KeyCode::Down,
        57354 => KeyCode::PageUp,
        57355 => KeyCode::PageDown,
        57356 => KeyCode::Home,
        57357 => KeyCode::End,
        57358 => KeyCode::CapsLock,
        57359 => KeyCode::ScrollLock,
        57360 => KeyCode::NumLock,
        57361 => KeyCode::PrintScreen,
        57362 => KeyCode::Pause,
        57363 => KeyCode::Menu,
        // Kitty F1–F35 (57364..=57398). Range guarantees result fits in u8.
        #[allow(clippy::cast_possible_truncation)]
        cp @ 57364..=57398 => KeyCode::F((cp - 57364 + 1) as u8),
        // Regular Unicode character.
        cp => char::from_u32(u32::from(cp)).map_or(
            KeyCode::Char('\0'),
            KeyCode::Char,
        ),
    }
}

/// Expected byte length of a UTF-8 character from its lead byte.
/// Returns 0 for invalid lead bytes (continuation bytes, 0xFE, 0xFF).
const fn utf8_char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 0,
    }
}

/// Find the first occurrence of `needle` in `haystack`.
/// Returns the byte offset of the start of the match.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse bytes and return all events.
    fn parse(data: &[u8]) -> Vec<Event> {
        Parser::new().advance(data)
    }

    /// Helper: parse bytes, return exactly one event.
    fn parse_one(data: &[u8]) -> Event {
        let events = parse(data);
        assert_eq!(
            events.len(),
            1,
            "expected 1 event, got {}: {:?}",
            events.len(),
            events
        );
        events.into_iter().next().unwrap()
    }

    /// Helper: build a simple key press event.
    fn key(code: KeyCode) -> Event {
        press(code)
    }

    /// Helper: build a key event with modifiers.
    fn key_mod(code: KeyCode, modifiers: Modifiers) -> Event {
        key_with(code, modifiers)
    }

    // ── ASCII Printable ─────────────────────────────────────────────────

    #[test]
    fn ascii_single_char() {
        assert_eq!(parse_one(b"a"), key(KeyCode::Char('a')));
    }

    #[test]
    fn ascii_multiple_chars() {
        let events = parse(b"abc");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], key(KeyCode::Char('a')));
        assert_eq!(events[1], key(KeyCode::Char('b')));
        assert_eq!(events[2], key(KeyCode::Char('c')));
    }

    #[test]
    fn ascii_space() {
        assert_eq!(parse_one(b" "), key(KeyCode::Char(' ')));
    }

    #[test]
    fn ascii_tilde() {
        assert_eq!(parse_one(b"~"), key(KeyCode::Char('~')));
    }

    #[test]
    fn ascii_digits() {
        let events = parse(b"0123456789");
        assert_eq!(events.len(), 10);
        for (i, event) in events.iter().enumerate() {
            let ch = char::from_digit(i as u32, 10).unwrap();
            assert_eq!(*event, key(KeyCode::Char(ch)));
        }
    }

    // ── Control Characters ──────────────────────────────────────────────

    #[test]
    fn ctrl_a() {
        assert_eq!(
            parse_one(b"\x01"),
            key_mod(KeyCode::Char('a'), Modifiers::CTRL)
        );
    }

    #[test]
    fn ctrl_c() {
        assert_eq!(
            parse_one(b"\x03"),
            key_mod(KeyCode::Char('c'), Modifiers::CTRL)
        );
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(
            parse_one(b"\x1A"),
            key_mod(KeyCode::Char('z'), Modifiers::CTRL)
        );
    }

    #[test]
    fn ctrl_at() {
        assert_eq!(
            parse_one(b"\x00"),
            key_mod(KeyCode::Char('@'), Modifiers::CTRL)
        );
    }

    #[test]
    fn enter_cr() {
        assert_eq!(parse_one(b"\r"), key(KeyCode::Enter));
    }

    #[test]
    fn enter_lf() {
        assert_eq!(parse_one(b"\n"), key(KeyCode::Enter));
    }

    #[test]
    fn tab() {
        assert_eq!(parse_one(b"\t"), key(KeyCode::Tab));
    }

    #[test]
    fn backspace_0x08() {
        assert_eq!(parse_one(b"\x08"), key(KeyCode::Backspace));
    }

    #[test]
    fn backspace_0x7f() {
        assert_eq!(parse_one(b"\x7F"), key(KeyCode::Backspace));
    }

    // ── Arrow Keys (CSI) ────────────────────────────────────────────────

    #[test]
    fn arrow_up() {
        assert_eq!(parse_one(b"\x1b[A"), key(KeyCode::Up));
    }

    #[test]
    fn arrow_down() {
        assert_eq!(parse_one(b"\x1b[B"), key(KeyCode::Down));
    }

    #[test]
    fn arrow_right() {
        assert_eq!(parse_one(b"\x1b[C"), key(KeyCode::Right));
    }

    #[test]
    fn arrow_left() {
        assert_eq!(parse_one(b"\x1b[D"), key(KeyCode::Left));
    }

    // ── Arrow Keys with Modifiers ───────────────────────────────────────

    #[test]
    fn shift_up() {
        assert_eq!(
            parse_one(b"\x1b[1;2A"),
            key_mod(KeyCode::Up, Modifiers::SHIFT)
        );
    }

    #[test]
    fn alt_down() {
        assert_eq!(
            parse_one(b"\x1b[1;3B"),
            key_mod(KeyCode::Down, Modifiers::ALT)
        );
    }

    #[test]
    fn ctrl_right() {
        assert_eq!(
            parse_one(b"\x1b[1;5C"),
            key_mod(KeyCode::Right, Modifiers::CTRL)
        );
    }

    #[test]
    fn shift_alt_left() {
        assert_eq!(
            parse_one(b"\x1b[1;4D"),
            key_mod(KeyCode::Left, Modifiers::SHIFT | Modifiers::ALT)
        );
    }

    #[test]
    fn ctrl_shift_up() {
        assert_eq!(
            parse_one(b"\x1b[1;6A"),
            key_mod(KeyCode::Up, Modifiers::SHIFT | Modifiers::CTRL)
        );
    }

    // ── Navigation Keys ─────────────────────────────────────────────────

    #[test]
    fn home_csi_h() {
        assert_eq!(parse_one(b"\x1b[H"), key(KeyCode::Home));
    }

    #[test]
    fn end_csi_f() {
        assert_eq!(parse_one(b"\x1b[F"), key(KeyCode::End));
    }

    #[test]
    fn home_csi_tilde() {
        assert_eq!(parse_one(b"\x1b[1~"), key(KeyCode::Home));
    }

    #[test]
    fn insert() {
        assert_eq!(parse_one(b"\x1b[2~"), key(KeyCode::Insert));
    }

    #[test]
    fn delete() {
        assert_eq!(parse_one(b"\x1b[3~"), key(KeyCode::Delete));
    }

    #[test]
    fn end_csi_tilde() {
        assert_eq!(parse_one(b"\x1b[4~"), key(KeyCode::End));
    }

    #[test]
    fn page_up() {
        assert_eq!(parse_one(b"\x1b[5~"), key(KeyCode::PageUp));
    }

    #[test]
    fn page_down() {
        assert_eq!(parse_one(b"\x1b[6~"), key(KeyCode::PageDown));
    }

    // ── Navigation with Modifiers ───────────────────────────────────────

    #[test]
    fn ctrl_delete() {
        assert_eq!(
            parse_one(b"\x1b[3;5~"),
            key_mod(KeyCode::Delete, Modifiers::CTRL)
        );
    }

    #[test]
    fn shift_insert() {
        assert_eq!(
            parse_one(b"\x1b[2;2~"),
            key_mod(KeyCode::Insert, Modifiers::SHIFT)
        );
    }

    // ── Function Keys (SS3) ─────────────────────────────────────────────

    #[test]
    fn f1_ss3() {
        assert_eq!(parse_one(b"\x1bOP"), key(KeyCode::F(1)));
    }

    #[test]
    fn f2_ss3() {
        assert_eq!(parse_one(b"\x1bOQ"), key(KeyCode::F(2)));
    }

    #[test]
    fn f3_ss3() {
        assert_eq!(parse_one(b"\x1bOR"), key(KeyCode::F(3)));
    }

    #[test]
    fn f4_ss3() {
        assert_eq!(parse_one(b"\x1bOS"), key(KeyCode::F(4)));
    }

    // ── Function Keys (CSI) ─────────────────────────────────────────────

    #[test]
    fn f1_csi() {
        assert_eq!(parse_one(b"\x1b[P"), key(KeyCode::F(1)));
    }

    #[test]
    fn f5() {
        assert_eq!(parse_one(b"\x1b[15~"), key(KeyCode::F(5)));
    }

    #[test]
    fn f6() {
        assert_eq!(parse_one(b"\x1b[17~"), key(KeyCode::F(6)));
    }

    #[test]
    fn f7() {
        assert_eq!(parse_one(b"\x1b[18~"), key(KeyCode::F(7)));
    }

    #[test]
    fn f8() {
        assert_eq!(parse_one(b"\x1b[19~"), key(KeyCode::F(8)));
    }

    #[test]
    fn f9() {
        assert_eq!(parse_one(b"\x1b[20~"), key(KeyCode::F(9)));
    }

    #[test]
    fn f10() {
        assert_eq!(parse_one(b"\x1b[21~"), key(KeyCode::F(10)));
    }

    #[test]
    fn f11() {
        assert_eq!(parse_one(b"\x1b[23~"), key(KeyCode::F(11)));
    }

    #[test]
    fn f12() {
        assert_eq!(parse_one(b"\x1b[24~"), key(KeyCode::F(12)));
    }

    // ── Function Keys with Modifiers ────────────────────────────────────

    #[test]
    fn shift_f5() {
        assert_eq!(
            parse_one(b"\x1b[15;2~"),
            key_mod(KeyCode::F(5), Modifiers::SHIFT)
        );
    }

    #[test]
    fn ctrl_f12() {
        assert_eq!(
            parse_one(b"\x1b[24;5~"),
            key_mod(KeyCode::F(12), Modifiers::CTRL)
        );
    }

    // ── Shift+Tab ───────────────────────────────────────────────────────

    #[test]
    fn shift_tab() {
        assert_eq!(
            parse_one(b"\x1b[Z"),
            key_mod(KeyCode::Tab, Modifiers::SHIFT)
        );
    }

    // ── Alt + Key ───────────────────────────────────────────────────────

    #[test]
    fn alt_a() {
        assert_eq!(
            parse_one(b"\x1ba"),
            key_mod(KeyCode::Char('a'), Modifiers::ALT)
        );
    }

    #[test]
    fn alt_z() {
        assert_eq!(
            parse_one(b"\x1bz"),
            key_mod(KeyCode::Char('z'), Modifiers::ALT)
        );
    }

    #[test]
    fn alt_space() {
        assert_eq!(
            parse_one(b"\x1b "),
            key_mod(KeyCode::Char(' '), Modifiers::ALT)
        );
    }

    #[test]
    fn alt_escape() {
        assert_eq!(
            parse_one(b"\x1b\x1b"),
            key_mod(KeyCode::Escape, Modifiers::ALT)
        );
    }

    #[test]
    fn alt_ctrl_a() {
        assert_eq!(
            parse_one(b"\x1b\x01"),
            key_mod(
                KeyCode::Char('a'),
                Modifiers::ALT | Modifiers::CTRL
            )
        );
    }

    // ── SS3 Navigation ──────────────────────────────────────────────────

    #[test]
    fn ss3_arrow_up() {
        assert_eq!(parse_one(b"\x1bOA"), key(KeyCode::Up));
    }

    #[test]
    fn ss3_home() {
        assert_eq!(parse_one(b"\x1bOH"), key(KeyCode::Home));
    }

    #[test]
    fn ss3_end() {
        assert_eq!(parse_one(b"\x1bOF"), key(KeyCode::End));
    }

    // ── SGR Mouse: Press/Release ────────────────────────────────────────

    #[test]
    fn mouse_left_press() {
        assert_eq!(
            parse_one(b"\x1b[<0;10;20M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_left_release() {
        assert_eq!(
            parse_one(b"\x1b[<0;10;20m"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Release(MouseButton::Left),
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_middle_press() {
        assert_eq!(
            parse_one(b"\x1b[<1;5;5M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Middle),
                x: 4,
                y: 4,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_right_press() {
        assert_eq!(
            parse_one(b"\x1b[<2;1;1M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Right),
                x: 0,
                y: 0,
                modifiers: Modifiers::empty(),
            })
        );
    }

    // ── SGR Mouse: Scroll ───────────────────────────────────────────────

    #[test]
    fn mouse_scroll_up() {
        assert_eq!(
            parse_one(b"\x1b[<64;10;20M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_scroll_down() {
        assert_eq!(
            parse_one(b"\x1b[<65;10;20M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_scroll_left() {
        assert_eq!(
            parse_one(b"\x1b[<66;10;20M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollLeft,
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_scroll_right() {
        assert_eq!(
            parse_one(b"\x1b[<67;10;20M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollRight,
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })
        );
    }

    // ── SGR Mouse: Motion/Drag ──────────────────────────────────────────

    #[test]
    fn mouse_left_drag() {
        // Drag = motion bit (32) + button 0 = 32.
        assert_eq!(
            parse_one(b"\x1b[<32;15;25M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                x: 14,
                y: 24,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_right_drag() {
        // Drag = motion bit (32) + button 2 = 34.
        assert_eq!(
            parse_one(b"\x1b[<34;15;25M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Right),
                x: 14,
                y: 24,
                modifiers: Modifiers::empty(),
            })
        );
    }

    #[test]
    fn mouse_move_no_button() {
        // Motion with "button 3" (no button) = 32 + 3 = 35.
        assert_eq!(
            parse_one(b"\x1b[<35;15;25M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Move,
                x: 14,
                y: 24,
                modifiers: Modifiers::empty(),
            })
        );
    }

    // ── SGR Mouse: Modifiers ────────────────────────────────────────────

    #[test]
    fn mouse_shift_click() {
        // Shift = bit 2 (value 4), left button = 0: 0+4 = 4.
        assert_eq!(
            parse_one(b"\x1b[<4;10;10M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 9,
                y: 9,
                modifiers: Modifiers::SHIFT,
            })
        );
    }

    #[test]
    fn mouse_ctrl_click() {
        // Ctrl = bit 4 (value 16), left button = 0: 0+16 = 16.
        assert_eq!(
            parse_one(b"\x1b[<16;10;10M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 9,
                y: 9,
                modifiers: Modifiers::CTRL,
            })
        );
    }

    #[test]
    fn mouse_alt_click() {
        // Alt = bit 3 (value 8), left button = 0: 0+8 = 8.
        assert_eq!(
            parse_one(b"\x1b[<8;10;10M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 9,
                y: 9,
                modifiers: Modifiers::ALT,
            })
        );
    }

    // ── SGR Mouse: Large Coordinates ────────────────────────────────────

    #[test]
    fn mouse_large_coordinates() {
        // SGR supports coords > 223 (unlike X10 protocol).
        assert_eq!(
            parse_one(b"\x1b[<0;300;150M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 299,
                y: 149,
                modifiers: Modifiers::empty(),
            })
        );
    }

    // ── Focus Events ────────────────────────────────────────────────────

    #[test]
    fn focus_gained() {
        assert_eq!(parse_one(b"\x1b[I"), Event::FocusGained);
    }

    #[test]
    fn focus_lost() {
        assert_eq!(parse_one(b"\x1b[O"), Event::FocusLost);
    }

    // ── Kitty Keyboard Protocol ─────────────────────────────────────────

    #[test]
    fn kitty_char_a() {
        assert_eq!(parse_one(b"\x1b[97u"), key(KeyCode::Char('a')));
    }

    #[test]
    fn kitty_enter() {
        assert_eq!(parse_one(b"\x1b[13u"), key(KeyCode::Enter));
    }

    #[test]
    fn kitty_tab() {
        assert_eq!(parse_one(b"\x1b[9u"), key(KeyCode::Tab));
    }

    #[test]
    fn kitty_escape() {
        assert_eq!(parse_one(b"\x1b[27u"), key(KeyCode::Escape));
    }

    #[test]
    fn kitty_backspace() {
        assert_eq!(parse_one(b"\x1b[127u"), key(KeyCode::Backspace));
    }

    #[test]
    fn kitty_shift_a() {
        assert_eq!(
            parse_one(b"\x1b[97;2u"),
            key_mod(KeyCode::Char('a'), Modifiers::SHIFT)
        );
    }

    #[test]
    fn kitty_ctrl_a() {
        assert_eq!(
            parse_one(b"\x1b[97;5u"),
            key_mod(KeyCode::Char('a'), Modifiers::CTRL)
        );
    }

    #[test]
    fn kitty_alt_a() {
        assert_eq!(
            parse_one(b"\x1b[97;3u"),
            key_mod(KeyCode::Char('a'), Modifiers::ALT)
        );
    }

    #[test]
    fn kitty_ctrl_shift_a() {
        assert_eq!(
            parse_one(b"\x1b[97;6u"),
            key_mod(KeyCode::Char('a'), Modifiers::SHIFT | Modifiers::CTRL)
        );
    }

    #[test]
    fn kitty_super_modifier() {
        assert_eq!(
            parse_one(b"\x1b[97;9u"),
            key_mod(KeyCode::Char('a'), Modifiers::SUPER)
        );
    }

    #[test]
    fn kitty_key_release() {
        // Event type 3 = release, colon sub-param: modifiers:event_type.
        assert_eq!(
            parse_one(b"\x1b[97;1:3u"),
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::empty(),
                kind: KeyEventKind::Release,
            })
        );
    }

    #[test]
    fn kitty_key_repeat() {
        assert_eq!(
            parse_one(b"\x1b[97;1:2u"),
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::empty(),
                kind: KeyEventKind::Repeat,
            })
        );
    }

    #[test]
    fn kitty_shift_release() {
        // Modifier 2 (Shift), event type 3 (release) → "2:3".
        assert_eq!(
            parse_one(b"\x1b[97;2:3u"),
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::SHIFT,
                kind: KeyEventKind::Release,
            })
        );
    }

    // ── Kitty Functional Key Codepoints ─────────────────────────────────

    #[test]
    fn kitty_insert() {
        assert_eq!(parse_one(b"\x1b[57348u"), key(KeyCode::Insert));
    }

    #[test]
    fn kitty_delete() {
        assert_eq!(parse_one(b"\x1b[57349u"), key(KeyCode::Delete));
    }

    #[test]
    fn kitty_arrow_left() {
        assert_eq!(parse_one(b"\x1b[57350u"), key(KeyCode::Left));
    }

    #[test]
    fn kitty_arrow_right() {
        assert_eq!(parse_one(b"\x1b[57351u"), key(KeyCode::Right));
    }

    #[test]
    fn kitty_arrow_up() {
        assert_eq!(parse_one(b"\x1b[57352u"), key(KeyCode::Up));
    }

    #[test]
    fn kitty_arrow_down() {
        assert_eq!(parse_one(b"\x1b[57353u"), key(KeyCode::Down));
    }

    #[test]
    fn kitty_page_up() {
        assert_eq!(parse_one(b"\x1b[57354u"), key(KeyCode::PageUp));
    }

    #[test]
    fn kitty_page_down() {
        assert_eq!(parse_one(b"\x1b[57355u"), key(KeyCode::PageDown));
    }

    #[test]
    fn kitty_home() {
        assert_eq!(parse_one(b"\x1b[57356u"), key(KeyCode::Home));
    }

    #[test]
    fn kitty_end() {
        assert_eq!(parse_one(b"\x1b[57357u"), key(KeyCode::End));
    }

    #[test]
    fn kitty_caps_lock() {
        assert_eq!(parse_one(b"\x1b[57358u"), key(KeyCode::CapsLock));
    }

    #[test]
    fn kitty_f1() {
        assert_eq!(parse_one(b"\x1b[57364u"), key(KeyCode::F(1)));
    }

    #[test]
    fn kitty_f12() {
        assert_eq!(parse_one(b"\x1b[57375u"), key(KeyCode::F(12)));
    }

    // ── Bracketed Paste ─────────────────────────────────────────────────

    #[test]
    fn paste_simple() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[200~hello world\x1b[201~");
        assert_eq!(events, [Event::Paste("hello world".into())]);
    }

    #[test]
    fn paste_with_special_chars() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[200~line1\nline2\ttab\x1b[201~");
        assert_eq!(events, [Event::Paste("line1\nline2\ttab".into())]);
    }

    #[test]
    fn paste_split_across_chunks() {
        let mut parser = Parser::new();
        let events1 = parser.advance(b"\x1b[200~hel");
        assert!(events1.is_empty());
        assert!(parser.has_pending());

        let events2 = parser.advance(b"lo\x1b[201~");
        assert_eq!(events2, [Event::Paste("hello".into())]);
    }

    #[test]
    fn paste_empty() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[200~\x1b[201~");
        assert_eq!(events, [Event::Paste(String::new())]);
    }

    #[test]
    fn paste_followed_by_key() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[200~text\x1b[201~a");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], Event::Paste("text".into()));
        assert_eq!(events[1], key(KeyCode::Char('a')));
    }

    // ── UTF-8 ───────────────────────────────────────────────────────────

    #[test]
    fn utf8_two_byte() {
        // é = U+00E9 = 0xC3 0xA9.
        assert_eq!(parse_one(&[0xC3, 0xA9]), key(KeyCode::Char('é')));
    }

    #[test]
    fn utf8_three_byte() {
        // 中 = U+4E2D = 0xE4 0xB8 0xAD.
        assert_eq!(
            parse_one(&[0xE4, 0xB8, 0xAD]),
            key(KeyCode::Char('中'))
        );
    }

    #[test]
    fn utf8_four_byte() {
        // 🦀 = U+1F980 = 0xF0 0x9F 0xA6 0x80.
        assert_eq!(
            parse_one(&[0xF0, 0x9F, 0xA6, 0x80]),
            key(KeyCode::Char('🦀'))
        );
    }

    #[test]
    fn utf8_incomplete_waits() {
        let mut parser = Parser::new();
        let events = parser.advance(&[0xE4]);
        assert!(events.is_empty());
        assert!(parser.has_pending());

        let events = parser.advance(&[0xB8, 0xAD]);
        assert_eq!(events, [key(KeyCode::Char('中'))]);
    }

    // ── Escape Timeout (flush) ──────────────────────────────────────────

    #[test]
    fn lone_escape_pending() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b");
        assert!(events.is_empty());
        assert!(parser.has_pending());
    }

    #[test]
    fn lone_escape_flushed() {
        let mut parser = Parser::new();
        parser.advance(b"\x1b");
        let events = parser.flush();
        assert_eq!(events, [key(KeyCode::Escape)]);
        assert!(!parser.has_pending());
    }

    // ── Incremental Parsing ─────────────────────────────────────────────

    #[test]
    fn split_escape_sequence() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[");
        assert!(events.is_empty());

        let events = parser.advance(b"A");
        assert_eq!(events, [key(KeyCode::Up)]);
    }

    #[test]
    fn split_sgr_mouse() {
        let mut parser = Parser::new();
        let events = parser.advance(b"\x1b[<0;10");
        assert!(events.is_empty());

        let events = parser.advance(b";20M");
        assert_eq!(
            events,
            [Event::Mouse(MouseEvent {
                kind: MouseEventKind::Press(MouseButton::Left),
                x: 9,
                y: 19,
                modifiers: Modifiers::empty(),
            })]
        );
    }

    // ── Mixed Input ─────────────────────────────────────────────────────

    #[test]
    fn interleaved_keys_and_mouse() {
        let events = parse(b"a\x1b[<0;5;5Mb");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], key(KeyCode::Char('a')));
        assert!(matches!(events[1], Event::Mouse(_)));
        assert_eq!(events[2], key(KeyCode::Char('b')));
    }

    #[test]
    fn rapid_arrow_keys() {
        let events = parse(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], key(KeyCode::Up));
        assert_eq!(events[1], key(KeyCode::Down));
        assert_eq!(events[2], key(KeyCode::Right));
        assert_eq!(events[3], key(KeyCode::Left));
    }

    // ── Modifier Decoding ───────────────────────────────────────────────

    #[test]
    fn decode_modifier_none() {
        assert_eq!(decode_modifiers(0), Modifiers::empty());
        assert_eq!(decode_modifiers(1), Modifiers::empty());
    }

    #[test]
    fn decode_modifier_shift() {
        assert_eq!(decode_modifiers(2), Modifiers::SHIFT);
    }

    #[test]
    fn decode_modifier_alt() {
        assert_eq!(decode_modifiers(3), Modifiers::ALT);
    }

    #[test]
    fn decode_modifier_ctrl() {
        assert_eq!(decode_modifiers(5), Modifiers::CTRL);
    }

    #[test]
    fn decode_modifier_shift_alt() {
        assert_eq!(decode_modifiers(4), Modifiers::SHIFT | Modifiers::ALT);
    }

    #[test]
    fn decode_modifier_ctrl_shift() {
        assert_eq!(decode_modifiers(6), Modifiers::SHIFT | Modifiers::CTRL);
    }

    #[test]
    fn decode_modifier_super() {
        assert_eq!(decode_modifiers(9), Modifiers::SUPER);
    }

    // ── Number Parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_u16_basic() {
        assert_eq!(parse_u16_at(b"123", 0), (123, 3));
    }

    #[test]
    fn parse_u16_stops_at_non_digit() {
        assert_eq!(parse_u16_at(b"42;7", 0), (42, 2));
    }

    #[test]
    fn parse_u16_empty() {
        assert_eq!(parse_u16_at(b"", 0), (0, 0));
    }

    #[test]
    fn parse_u16_at_offset() {
        assert_eq!(parse_u16_at(b"ab123cd", 2), (123, 5));
    }

    #[test]
    fn parse_u16_saturates() {
        let (val, _) = parse_u16_at(b"99999", 0);
        assert_eq!(val, u16::MAX);
    }

    // ── CSI Parameter Parsing ───────────────────────────────────────────

    #[test]
    fn csi_params_empty() {
        assert!(parse_csi_params(b"").is_empty());
    }

    #[test]
    fn csi_params_single() {
        let params = parse_csi_params(b"42");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].0, 42);
        assert_eq!(params[0].1, 0);
    }

    #[test]
    fn csi_params_multiple() {
        let params = parse_csi_params(b"1;2;3");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].0, 1);
        assert_eq!(params[1].0, 2);
        assert_eq!(params[2].0, 3);
    }

    #[test]
    fn csi_params_with_sub_param() {
        let params = parse_csi_params(b"97;5:2");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].0, 97);
        assert_eq!(params[0].1, 0);
        assert_eq!(params[1].0, 5);
        assert_eq!(params[1].1, 2);
    }

    // ── UTF-8 Length ────────────────────────────────────────────────────

    #[test]
    fn utf8_len_ascii() {
        assert_eq!(utf8_char_len(b'a'), 1);
    }

    #[test]
    fn utf8_len_two_byte() {
        assert_eq!(utf8_char_len(0xC3), 2);
    }

    #[test]
    fn utf8_len_three_byte() {
        assert_eq!(utf8_char_len(0xE4), 3);
    }

    #[test]
    fn utf8_len_four_byte() {
        assert_eq!(utf8_char_len(0xF0), 4);
    }

    #[test]
    fn utf8_len_continuation_invalid() {
        assert_eq!(utf8_char_len(0x80), 0);
        assert_eq!(utf8_char_len(0xBF), 0);
    }
}
