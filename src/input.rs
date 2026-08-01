use crate::signal;
use crate::term::STDIN_FD;
use std::os::fd::{BorrowedFd, RawFd};

const PASTE_LIMIT: usize = 1024; // 1 KiB — reject pastes larger than this
const READ_BUF_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Tab,
    Backspace,
    Delete,
    Enter,
    Escape,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    #[allow(dead_code)]
    pub shift: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
}

impl KeyEvent {
    pub fn key(key: Key) -> Self {
        Self {
            key,
            mods: Modifiers::NONE,
        }
    }

    pub fn char(c: char) -> Self {
        Self {
            key: Key::Char(c),
            mods: Modifiers::NONE,
        }
    }

    pub fn ctrl(c: char) -> Self {
        Self {
            key: Key::Char(c),
            mods: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        }
    }

    pub fn alt(c: char) -> Self {
        Self {
            key: Key::Char(c),
            mods: Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        }
    }

    pub fn with_mods(key: Key, mods: Modifiers) -> Self {
        Self { key, mods }
    }
}

pub enum InputEvent {
    Key(KeyEvent),
    Signal(i32),
    /// A bracketed paste under the 1 KiB limit. Contains the full pasted text.
    /// The caller should insert it into the line buffer in one shot.
    Paste(String),
    /// A bracketed paste exceeded the 1 KiB limit and was rejected.
    /// The caller should replace the line buffer with an error message.
    PasteRejected,
}

pub struct InputReader {
    signal_fd: RawFd,
    in_paste: bool,
    paste_buf: String,
    paste_limit_hit: bool,
    buf: [u8; READ_BUF_SIZE],
    buf_pos: usize,
    buf_end: usize,
}

enum PollResult {
    Stdin,
    Signal,
    Timeout,
    /// stdin fd has POLLHUP without POLLIN, or polling failed fatally — treat as EOF.
    StdinHup,
}

impl InputReader {
    pub fn new(signal_fd: RawFd) -> Self {
        Self {
            signal_fd,
            in_paste: false,
            paste_buf: String::new(),
            paste_limit_hit: false,
            buf: [0u8; READ_BUF_SIZE],
            buf_pos: 0,
            buf_end: 0,
        }
    }

    /// True while inside a bracketed paste (`\x1b[200~` … `\x1b[201~`).
    pub fn in_paste(&self) -> bool {
        self.in_paste
    }

    /// Accumulate a single key event into the in-progress paste buffer,
    /// tracking the byte limit.  Once exceeded the buffer is discarded and
    /// `paste_limit_hit` is set so remaining bytes are suppressed.
    fn accumulate_paste_key(&mut self, key: KeyEvent) {
        if self.paste_limit_hit {
            return;
        }
        match key.key {
            Key::Char(c) => self.paste_buf.push(c),
            Key::Enter => self.paste_buf.push('\n'),
            Key::Tab => self.paste_buf.push('\t'),
            _ => {}
        }
        if self.paste_buf.len() > PASTE_LIMIT {
            self.paste_limit_hit = true;
            self.paste_buf.clear();
        }
    }

    /// Block until a key or signal event is available.
    ///
    /// During a bracketed paste, all key events are accumulated into a single
    /// `String` and returned as `Paste(String)` when the paste ends — no
    /// per-character events are dispatched, so the caller can insert the
    /// entire text in one shot and render once.
    pub fn read_event(&mut self) -> InputEvent {
        loop {
            // Drain buffered data without polling the kernel for every byte.
            while self.buf_pos < self.buf_end {
                if let Some(key) = self.decode_key() {
                    if self.in_paste {
                        self.accumulate_paste_key(key);
                        if self.paste_limit_hit {
                            break; // limit hit mid-buffer — skip rest below
                        }
                        continue;
                    }
                    return InputEvent::Key(key);
                }
                // decode_key returned None (CSI 200~/201~, suppressed byte, etc.)
                if !self.in_paste {
                    break;
                }
            }

            // Paste under limit and just ended — return accumulated text.
            if !self.in_paste && !self.paste_buf.is_empty() {
                self.paste_limit_hit = false;
                return InputEvent::Paste(std::mem::take(&mut self.paste_buf));
            }

            // Paste exceeded limit — scan raw bytes for the paste-end marker
            // (\x1b[201~) without per-byte decode_key overhead.  This blocks
            // in read() until the marker is found or EOF, so no polling needed.
            if self.in_paste && self.paste_limit_hit {
                self.skip_paste_remainder();
                self.paste_limit_hit = false;
                return InputEvent::PasteRejected;
            }

            match self.poll(-1) {
                PollResult::Stdin => {
                    if let Some(key) = self.decode_key() {
                        if self.in_paste {
                            self.accumulate_paste_key(key);
                            continue; // back to the drain loop
                        }
                        return InputEvent::Key(key);
                    }
                    if !self.in_paste {
                        if self.paste_limit_hit {
                            self.paste_limit_hit = false;
                            return InputEvent::PasteRejected;
                        }
                        if !self.paste_buf.is_empty() {
                            return InputEvent::Paste(std::mem::take(&mut self.paste_buf));
                        }
                    }
                }
                PollResult::Signal => {
                    if let Some(sig) = signal::read_signal() {
                        return InputEvent::Signal(sig);
                    }
                }
                PollResult::Timeout => {}
                PollResult::StdinHup => {
                    // PTY master closed (or stdin EOF) — synthesise Ctrl+D so the
                    // shell exits cleanly rather than spinning in poll forever.
                    return InputEvent::Key(KeyEvent::ctrl('d'));
                }
            }
        }
    }

    /// Like `read_event` but returns `None` if no event arrives within `timeout_ms`.
    pub fn read_event_timeout(&mut self, timeout_ms: i32) -> Option<InputEvent> {
        // Drain buffered data first so we don't poll when we already have bytes.
        while self.buf_pos < self.buf_end {
            if let Some(key) = self.decode_key() {
                if self.in_paste {
                    self.accumulate_paste_key(key);
                    if self.paste_limit_hit {
                        break; // limit hit mid-buffer — skip rest below
                    }
                    continue;
                }
                return Some(InputEvent::Key(key));
            }
            if !self.in_paste {
                break;
            }
        }

        if !self.in_paste && !self.paste_buf.is_empty() {
            self.paste_limit_hit = false;
            return Some(InputEvent::Paste(std::mem::take(&mut self.paste_buf)));
        }

        // Paste exceeded limit — scan raw bytes for paste-end.  Blocks in
        // read() until the marker is found, so no need to poll first.
        if self.in_paste && self.paste_limit_hit {
            self.skip_paste_remainder();
            self.paste_limit_hit = false;
            return Some(InputEvent::PasteRejected);
        }

        match self.poll(timeout_ms) {
            PollResult::Stdin => {
                if let Some(key) = self.decode_key() {
                    if self.in_paste {
                        self.accumulate_paste_key(key);
                        return None; // timeout variant — return None, next call continues
                    }
                    return Some(InputEvent::Key(key));
                }
                if !self.in_paste {
                    if self.paste_limit_hit {
                        self.paste_limit_hit = false;
                        return Some(InputEvent::PasteRejected);
                    }
                    if !self.paste_buf.is_empty() {
                        return Some(InputEvent::Paste(std::mem::take(&mut self.paste_buf)));
                    }
                }
            }
            PollResult::Signal => {
                if let Some(sig) = signal::read_signal() {
                    return Some(InputEvent::Signal(sig));
                }
            }
            PollResult::Timeout => {}
            PollResult::StdinHup => {
                return Some(InputEvent::Key(KeyEvent::ctrl('d')));
            }
        }
        None
    }

    fn poll(&self, timeout_ms: i32) -> PollResult {
        self.poll_fds(STDIN_FD, timeout_ms)
    }

    fn poll_fds(&self, stdin_fd: RawFd, timeout_ms: i32) -> PollResult {
        let stdin = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let signal = unsafe { BorrowedFd::borrow_raw(self.signal_fd) };
        let mut fds = [
            rustix::event::PollFd::from_borrowed_fd(stdin, rustix::event::PollFlags::IN),
            rustix::event::PollFd::from_borrowed_fd(signal, rustix::event::PollFlags::IN),
        ];
        loop {
            // SAFETY: poll on two valid fds (stdin + signal pipe). Returns
            // count of ready fds, 0 on timeout, -1 on error.
            let timeout = poll_timeout(timeout_ms);
            let n = match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(n) => n,
                Err(e) if e == rustix::io::Errno::INTR => continue,
                Err(_) => return PollResult::StdinHup,
            };
            if n == 0 {
                return PollResult::Timeout;
            }
            if fds[1].revents().contains(rustix::event::PollFlags::IN) {
                return PollResult::Signal;
            }
            if fds[0].revents().contains(rustix::event::PollFlags::IN) {
                return PollResult::Stdin;
            }
            // POLLHUP on stdin without POLLIN: PTY master was closed (or similar).
            // Treat as EOF so the shell exits rather than spinning.
            if fds[0].revents().contains(rustix::event::PollFlags::HUP) {
                return PollResult::StdinHup;
            }
            return PollResult::StdinHup;
        }
    }

    /// Returns true if stdin has data available to read immediately.
    pub fn has_pending_input(&self) -> bool {
        self.buf_pos < self.buf_end || self.poll_stdin(0)
    }

    /// Returns true if the internal buffer has data or stdin is readable within
    /// `timeout_ms` milliseconds.
    fn poll_stdin(&self, timeout_ms: i32) -> bool {
        // Data already buffered — no need to ask the kernel.
        if self.buf_pos < self.buf_end {
            return true;
        }
        let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
        let mut fds = [rustix::event::PollFd::from_borrowed_fd(
            stdin,
            rustix::event::PollFlags::IN,
        )];
        // SAFETY: poll on a single valid fd (stdin). Stack-allocated pollfd array.
        let timeout_ms = timeout_ms.max(0) as i64;
        let timeout = rustix::event::Timespec {
            tv_sec: timeout_ms / 1000,
            tv_nsec: (timeout_ms % 1000) * 1_000_000,
        };
        rustix::event::poll(&mut fds, Some(&timeout)).is_ok()
            && fds[0].revents().contains(rustix::event::PollFlags::IN)
    }

    /// Read up to `READ_BUF_SIZE` bytes from stdin into the internal buffer.
    /// Callers must have fully consumed the buffer (`buf_pos >= buf_end`) first.
    fn fill_buf(&mut self) -> Option<()> {
        debug_assert!(self.buf_pos >= self.buf_end);
        self.buf_pos = 0;
        self.buf_end = 0;
        let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
        match rustix::io::read(stdin, &mut self.buf) {
            Ok(0) => None,
            Ok(n) => {
                self.buf_end = n;
                Some(())
            }
            Err(_) => None,
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if self.buf_pos >= self.buf_end {
            self.fill_buf()?;
        }
        let byte = self.buf[self.buf_pos];
        self.buf_pos += 1;
        Some(byte)
    }

    fn read_byte_timeout(&mut self, timeout_ms: i32) -> Option<u8> {
        if self.poll_stdin(timeout_ms) {
            self.read_byte()
        } else {
            None
        }
    }

    /// Scan raw input buffer for the paste-end marker `\x1b[201~` and consume
    /// everything up to (and including) it.  When the marker is found,
    /// `in_paste` is set to false and the buffer position is advanced past it.
    /// If the paste-end may be split across the end of the current buffer
    /// (i.e. an ESC was found but there aren't enough bytes to check the
    /// full 6-byte marker), the bytes from ESC onward are kept at the front
    /// of the buffer and more data is read so the check can complete.
    /// Returns with `in_paste = false` when done (paste-end found or EOF).
    fn skip_paste_remainder(&mut self) {
        // paste-end = ESC [ 2 0 1 ~
        const PASTE_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

        loop {
            if self.buf_pos >= self.buf_end {
                // Poll for data before reading — on macOS PTY, read() can
                // return 0 even when more data is coming.  Poll gives a
                // reliable indication of whether stdin is truly at EOF.
                if !self.poll_stdin(100) {
                    // No data available within 100ms; keep waiting.
                    continue;
                }
                if self.fill_buf().is_none() {
                    // read() failed despite poll saying ready — truly EOF.
                    self.in_paste = false;
                    return;
                }
            } else {
                let remaining = &self.buf[self.buf_pos..self.buf_end];

                // memchr for ESC (0x1b) — first byte of paste-end.
                if let Some(esc_off) = remaining.iter().position(|&b| b == 0x1b) {
                    let after_esc = &remaining[esc_off..];
                    if after_esc.len() >= PASTE_END.len()
                        && after_esc[..PASTE_END.len()] == *PASTE_END
                    {
                        // Found the paste-end marker — consume it and exit paste.
                        self.buf_pos += esc_off + PASTE_END.len();
                        self.in_paste = false;
                        return;
                    }
                    // Not enough bytes to check the full marker — the paste-end
                    // may be split across a read boundary.  Keep the bytes from
                    // ESC onward at the front of the buffer and read more so we
                    // can check the complete 6-byte sequence on the next iteration.
                    if after_esc.len() < PASTE_END.len() {
                        let partial_len = after_esc.len();
                        self.buf_pos += esc_off;
                        // Move the partial bytes to the front of the buffer.
                        self.buf.copy_within(self.buf_pos..self.buf_end, 0);
                        self.buf_pos = 0;
                        self.buf_end = partial_len;
                        // Try to read more bytes after the partial sequence.
                        if !self.poll_stdin(100) {
                            continue; // keep waiting
                        }
                        let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
                        match rustix::io::read(stdin, &mut self.buf[self.buf_end..]) {
                            Ok(0) => {
                                self.in_paste = false;
                                return;
                            }
                            Ok(n) => {
                                self.buf_end += n;
                                // Loop again — now we should have enough bytes.
                                continue;
                            }
                            Err(_) => {
                                self.in_paste = false;
                                return;
                            }
                        }
                    }
                    // An ESC that isn't paste-end (and we had enough bytes to
                    // confirm that).  Skip it and continue scanning.
                    self.buf_pos += esc_off + 1;
                } else {
                    // No ESC in this buffer — discard it all and refill.
                    self.buf_pos = self.buf_end;
                }
            }
        }
    }

    fn decode_key(&mut self) -> Option<KeyEvent> {
        let byte = self.read_byte()?;

        match byte {
            0x00 => None,
            0x08 => Some(KeyEvent::with_mods(
                Key::Backspace,
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            )),
            0x09 => Some(KeyEvent::key(Key::Tab)),
            0x0a | 0x0d => Some(KeyEvent::key(Key::Enter)),
            0x1b => self.decode_escape(),
            0x7f => Some(KeyEvent::key(Key::Backspace)),
            // Ctrl+a through Ctrl+z (0x01-0x1a, excluding above)
            b @ 0x01..=0x1a => Some(KeyEvent::ctrl((b - 1 + b'a') as char)),
            b if b < 0x20 => None,
            b if b < 0x80 => Some(KeyEvent::char(b as char)),
            b => self.decode_utf8(b),
        }
    }

    fn decode_escape(&mut self) -> Option<KeyEvent> {
        match self.read_byte_timeout(50) {
            None => Some(KeyEvent::key(Key::Escape)),
            Some(b'[') => self.decode_csi(),
            Some(b'O') => self.decode_ss3(),
            Some(b'b') => Some(KeyEvent::alt('b')),
            Some(b'f') => Some(KeyEvent::alt('f')),
            Some(b'd') => Some(KeyEvent::alt('d')),
            Some(b) if (0x20..0x7f).contains(&b) => Some(KeyEvent::alt(b as char)),
            _ => Some(KeyEvent::key(Key::Escape)),
        }
    }

    fn decode_csi(&mut self) -> Option<KeyEvent> {
        let mut params = Vec::new();
        let mut current = 0u32;
        let mut has_digit = false;

        loop {
            let b = self.read_byte_timeout(50)?;
            match b {
                b'0'..=b'9' => {
                    current = current * 10 + (b - b'0') as u32;
                    has_digit = true;
                }
                b';' => {
                    params.push(if has_digit { current } else { 0 });
                    current = 0;
                    has_digit = false;
                }
                0x40..=0x7e => {
                    if has_digit {
                        params.push(current);
                    }
                    return self.csi_to_key(b, &params);
                }
                _ => return None,
            }
        }
    }

    fn csi_to_key(&mut self, final_byte: u8, params: &[u32]) -> Option<KeyEvent> {
        let mods = if params.len() >= 2 {
            modifier_from_param(params[1])
        } else {
            Modifiers::NONE
        };

        match final_byte {
            b'A' => Some(KeyEvent::with_mods(Key::Up, mods)),
            b'B' => Some(KeyEvent::with_mods(Key::Down, mods)),
            b'C' => Some(KeyEvent::with_mods(Key::Right, mods)),
            b'D' => Some(KeyEvent::with_mods(Key::Left, mods)),
            b'H' => Some(KeyEvent::with_mods(Key::Home, mods)),
            b'F' => Some(KeyEvent::with_mods(Key::End, mods)),
            b'Z' => Some(KeyEvent::with_mods(
                Key::Tab,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            )),
            b'~' => match params.first().copied().unwrap_or(0) {
                1 | 7 => Some(KeyEvent::with_mods(Key::Home, mods)),
                3 => Some(KeyEvent::with_mods(Key::Delete, mods)),
                4 | 8 => Some(KeyEvent::with_mods(Key::End, mods)),
                200 => {
                    self.in_paste = true;
                    self.paste_buf.clear();
                    self.paste_limit_hit = false;
                    None
                }
                201 => {
                    self.in_paste = false;
                    None
                }
                _ => None,
            },
            b'^' => match params.first().copied().unwrap_or(0) {
                3 => Some(KeyEvent::with_mods(
                    Key::Delete,
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    },
                )),
                _ => None,
            },
            _ => None,
        }
    }

    fn decode_ss3(&mut self) -> Option<KeyEvent> {
        let b = self.read_byte_timeout(50)?;
        match b {
            b'A' => Some(KeyEvent::key(Key::Up)),
            b'B' => Some(KeyEvent::key(Key::Down)),
            b'C' => Some(KeyEvent::key(Key::Right)),
            b'D' => Some(KeyEvent::key(Key::Left)),
            b'H' => Some(KeyEvent::key(Key::Home)),
            b'F' => Some(KeyEvent::key(Key::End)),
            _ => None,
        }
    }

    fn decode_utf8(&mut self, first: u8) -> Option<KeyEvent> {
        let (len, mut cp) = if first & 0xE0 == 0xC0 {
            (2, (first & 0x1F) as u32)
        } else if first & 0xF0 == 0xE0 {
            (3, (first & 0x0F) as u32)
        } else if first & 0xF8 == 0xF0 {
            (4, (first & 0x07) as u32)
        } else {
            return None;
        };

        for _ in 1..len {
            let b = self.read_byte()?;
            if b & 0xC0 != 0x80 {
                return None;
            }
            cp = (cp << 6) | (b & 0x3F) as u32;
        }

        char::from_u32(cp).map(KeyEvent::char)
    }
}

fn poll_timeout(timeout_ms: i32) -> Option<rustix::event::Timespec> {
    if timeout_ms < 0 {
        return None;
    }
    let timeout_ms = timeout_ms as i64;
    Some(rustix::event::Timespec {
        tv_sec: timeout_ms / 1000,
        tv_nsec: (timeout_ms % 1000) * 1_000_000,
    })
}

pub fn modifier_from_param(param: u32) -> Modifiers {
    let bits = param.saturating_sub(1);
    Modifiers {
        ctrl: bits & 4 != 0,
        alt: bits & 2 != 0,
        shift: bits & 1 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    #[test]
    fn modifier_parsing() {
        let m = modifier_from_param(5); // 1 + ctrl(4)
        assert!(m.ctrl);
        assert!(!m.alt);
        assert!(!m.shift);

        let m = modifier_from_param(3); // 1 + alt(2)
        assert!(!m.ctrl);
        assert!(m.alt);
        assert!(!m.shift);
    }

    #[test]
    fn csi_delete_with_ctrl_modifier_maps_to_ctrl_delete() {
        let mut reader = InputReader::new(-1);
        let key = reader.csi_to_key(b'~', &[3, 5]).expect("ctrl-delete");
        assert_eq!(key.key, Key::Delete);
        assert!(key.mods.ctrl);
        assert!(!key.mods.alt);
    }

    #[test]
    fn csi_delete_with_caret_suffix_maps_to_ctrl_delete() {
        let mut reader = InputReader::new(-1);
        let key = reader.csi_to_key(b'^', &[3]).expect("ctrl-delete");
        assert_eq!(key.key, Key::Delete);
        assert!(key.mods.ctrl);
        assert!(!key.mods.alt);
    }

    #[test]
    fn paste_start_end_resets_state() {
        let mut reader = InputReader::new(-1);

        assert!(!reader.in_paste());

        // CSI 200~ starts a paste and clears the accumulated buffer.
        assert!(reader.csi_to_key(b'~', &[200]).is_none());
        assert!(reader.in_paste());
        assert!(reader.paste_buf.is_empty());
        assert!(!reader.paste_limit_hit);

        // CSI 201~ ends the paste.
        assert!(reader.csi_to_key(b'~', &[201]).is_none());
        assert!(!reader.in_paste());
        assert!(!reader.paste_limit_hit);
    }

    #[test]
    fn paste_accumulation_stops_at_limit() {
        let mut reader = InputReader::new(-1);

        // Start paste.
        reader.csi_to_key(b'~', &[200]);
        assert!(reader.in_paste());

        // Accumulate characters up to PASTE_LIMIT — flag stays clear.
        for c in "a".repeat(PASTE_LIMIT).chars() {
            reader.accumulate_paste_key(KeyEvent::char(c));
        }
        assert_eq!(reader.paste_buf.len(), PASTE_LIMIT);
        assert!(!reader.paste_limit_hit);

        // One more character pushes us over.
        reader.accumulate_paste_key(KeyEvent::char('x'));
        assert!(reader.paste_buf.is_empty());
        assert!(reader.paste_limit_hit);

        // End paste — flag stays set until consumed by read_event.
        reader.csi_to_key(b'~', &[201]);
        assert!(!reader.in_paste());
        assert!(reader.paste_limit_hit);
    }

    #[test]
    fn blocking_poll_waits_for_signal() {
        let (stdin_read, _stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, signal_write) = rustix::pipe::pipe().unwrap();
        let reader = InputReader::new(signal_read.as_raw_fd());
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            rustix::io::write(&signal_write, b"x").unwrap();
        });

        let started = Instant::now();
        assert!(matches!(
            reader.poll_fds(stdin_read.as_raw_fd(), -1),
            PollResult::Signal
        ));
        assert!(started.elapsed() >= Duration::from_millis(25));
        writer.join().unwrap();
    }
}
