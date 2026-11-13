use crate::signal;
use crate::term::STDIN_FD;
use rustix::io::Errno;
use std::collections::VecDeque;
use std::os::fd::{BorrowedFd, RawFd};

const PASTE_LIMIT: usize = 8192;
const READ_BUF_SIZE: usize = 4096;
const ESCAPE_TIMEOUT_MS: i32 = 50;
const CSI_PARAMS: usize = 16;
const X10_MOUSE_BYTES: u8 = 3;

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
    Paste(String),
    PasteRejected,
}

pub struct InputReader {
    stdin_fd: RawFd,
    signal_fd: RawFd,
    in_paste: bool,
    paste_buf: String,
    paste_limit_hit: bool,
    buf: [u8; READ_BUF_SIZE],
    buf_pos: usize,
    buf_end: usize,
    pending: VecDeque<u8>,
    seq: Seq,
    csi: Csi,
    /// Set once a read proves the terminal is gone (EOF or an I/O error).
    /// Poll keeps reporting readiness on a hung-up descriptor forever, so
    /// without this the reader would retry the failing read without ever
    /// blocking.
    stdin_dead: bool,
}

enum PollResult {
    Stdin,
    Signal,
    Timeout,
    StdinHup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seq {
    Ground,
    Escape,
    Csi,
    Ss3,
    Str { esc: bool },
    Mouse { left: u8 },
    Utf8 { left: u8, cp: u32 },
}

#[derive(Clone, Copy)]
struct Csi {
    params: [u32; CSI_PARAMS],
    count: usize,
    current: u32,
    has_digit: bool,
    in_sub: bool,
    started: bool,
    private: bool,
    unsupported: bool,
}

impl Csi {
    const EMPTY: Self = Self {
        params: [0; CSI_PARAMS],
        count: 0,
        current: 0,
        has_digit: false,
        in_sub: false,
        started: false,
        private: false,
        unsupported: false,
    };

    fn digit(&mut self, b: u8) {
        self.started = true;
        self.has_digit = true;
        if self.in_sub {
            return;
        }
        match self.current.checked_mul(10).and_then(|v| v.checked_add((b - b'0') as u32)) {
            Some(value) => self.current = value,
            None => self.unsupported = true,
        }
    }

    fn separator(&mut self) {
        self.started = true;
        let value = if self.has_digit { self.current } else { 0 };
        self.push(value);
        self.current = 0;
        self.has_digit = false;
        self.in_sub = false;
    }

    fn finish(&mut self) {
        if self.has_digit {
            self.push(self.current);
        }
    }

    fn push(&mut self, value: u32) {
        if self.count < CSI_PARAMS {
            self.params[self.count] = value;
            self.count += 1;
        } else {
            self.unsupported = true;
        }
    }

    fn values(&self) -> &[u32] {
        &self.params[..self.count]
    }
}

enum Fed {
    Key(KeyEvent),
    Boundary,
    Pending,
}

enum SkipPasteResult {
    Complete,
    Signal,
}

impl InputReader {
    pub fn new(signal_fd: RawFd) -> Self {
        Self::with_fds(STDIN_FD, signal_fd)
    }

    fn with_fds(stdin_fd: RawFd, signal_fd: RawFd) -> Self {
        Self {
            stdin_fd,
            signal_fd,
            in_paste: false,
            paste_buf: String::new(),
            paste_limit_hit: false,
            buf: [0u8; READ_BUF_SIZE],
            buf_pos: 0,
            buf_end: 0,
            pending: VecDeque::new(),
            seq: Seq::Ground,
            csi: Csi::EMPTY,
            stdin_dead: false,
        }
    }

    /// Bytes to decode before the next read from the terminal. Used for input
    /// the capture relay took and could not deliver before the child exited.
    pub fn push_pending(&mut self, bytes: &[u8]) {
        self.pending.extend(bytes);
    }

    pub fn take_pending_bytes(&mut self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.pending.len() + (self.buf_end - self.buf_pos));
        if self.buf_pos < self.buf_end {
            result.extend_from_slice(&self.buf[self.buf_pos..self.buf_end]);
            self.buf_pos = 0;
            self.buf_end = 0;
        }
        result.extend(self.pending.drain(..));
        self.seq = Seq::Ground;
        result
    }

    pub fn in_paste(&self) -> bool {
        self.in_paste
    }

    fn accumulate_paste_key(&mut self, key: KeyEvent) {
        if self.paste_limit_hit {
            return;
        }
        match key.key {
            Key::Char(_) if key.mods.ctrl || key.mods.alt => {}
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

    pub fn read_event(&mut self) -> InputEvent {
        loop {
            while (self.buf_pos < self.buf_end || !self.pending.is_empty()) && !(self.in_paste && self.paste_limit_hit) {
                if let Some(key) = self.decode_key() {
                    if self.in_paste {
                        self.accumulate_paste_key(key);
                        if self.paste_limit_hit {
                            break;
                        }
                        continue;
                    }
                    return InputEvent::Key(key);
                }
                if !self.in_paste && !self.paste_buf.is_empty() {
                    break;
                }
            }

            if !self.in_paste && !self.paste_buf.is_empty() {
                self.paste_limit_hit = false;
                return InputEvent::Paste(std::mem::take(&mut self.paste_buf));
            }

            if self.in_paste && self.paste_limit_hit {
                match self.skip_paste_remainder() {
                    SkipPasteResult::Complete => {
                        self.paste_limit_hit = false;
                        return InputEvent::PasteRejected;
                    }
                    SkipPasteResult::Signal => {
                        if let Some(sig) = signal::read_signal() {
                            return InputEvent::Signal(sig);
                        }
                    }
                }
            }

            match self.poll(-1) {
                PollResult::Stdin => {
                    if let Some(key) = self.decode_key() {
                        if self.in_paste {
                            self.accumulate_paste_key(key);
                            continue;
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
                    return InputEvent::Key(KeyEvent::ctrl('d'));
                }
            }
        }
    }

    fn poll(&self, timeout_ms: i32) -> PollResult {
        self.poll_fds(self.stdin_fd, timeout_ms)
    }

    fn poll_fds(&self, stdin_fd: RawFd, timeout_ms: i32) -> PollResult {
        if self.stdin_dead {
            return PollResult::StdinHup;
        }
        // SAFETY: the input fd stays open for this call and the signal fd stays open with the reader.
        let stdin = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let signal = unsafe { BorrowedFd::borrow_raw(self.signal_fd) };
        let mut fds = [
            rustix::event::PollFd::from_borrowed_fd(stdin, rustix::event::PollFlags::IN),
            rustix::event::PollFd::from_borrowed_fd(signal, rustix::event::PollFlags::IN),
        ];
        loop {
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
            if fds[0].revents().contains(rustix::event::PollFlags::HUP) {
                return PollResult::StdinHup;
            }
            return PollResult::StdinHup;
        }
    }

    pub fn has_pending_input(&self) -> bool {
        self.buf_pos < self.buf_end || !self.pending.is_empty() || self.poll_stdin(0)
    }

    fn poll_stdin(&self, timeout_ms: i32) -> bool {
        if self.stdin_dead {
            return false;
        }
        if self.buf_pos < self.buf_end {
            return true;
        }
        // SAFETY: `stdin_fd` is borrowed and remains open for this reader's lifetime.
        let stdin = unsafe { BorrowedFd::borrow_raw(self.stdin_fd) };
        let mut fds = [rustix::event::PollFd::from_borrowed_fd(
            stdin,
            rustix::event::PollFlags::IN,
        )];
        let timeout_ms = timeout_ms.max(0) as i64;
        let timeout = rustix::event::Timespec {
            tv_sec: timeout_ms / 1000,
            tv_nsec: (timeout_ms % 1000) * 1_000_000,
        };
        rustix::event::poll(&mut fds, Some(&timeout)).is_ok()
            && fds[0].revents().contains(rustix::event::PollFlags::IN)
    }

    fn fill_buf(&mut self) -> Option<()> {
        debug_assert!(self.buf_pos >= self.buf_end);
        self.buf_pos = 0;
        self.buf_end = 0;
        // SAFETY: `stdin_fd` is borrowed and remains open for this reader's lifetime.
        let stdin = unsafe { BorrowedFd::borrow_raw(self.stdin_fd) };
        match rustix::io::read(stdin, &mut self.buf) {
            Ok(0) => {
                self.stdin_dead = true;
                None
            }
            Ok(n) => {
                self.buf_end = n;
                Some(())
            }
            Err(Errno::INTR) => None,
            Err(_) => {
                self.stdin_dead = true;
                None
            }
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if self.buf_pos >= self.buf_end {
            if let Some(byte) = self.pending.pop_front() {
                return Some(byte);
            }
            self.fill_buf()?;
        }
        let byte = self.buf[self.buf_pos];
        self.buf_pos += 1;
        Some(byte)
    }

    fn shift_pending_into_buf(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        if self.buf_pos < self.buf_end && self.buf_end >= self.buf.len() {
            return false;
        }
        if self.buf_pos >= self.buf_end {
            self.buf_pos = 0;
            self.buf_end = 0;
        }
        let space = self.buf.len() - self.buf_end;
        let mut copied = 0;
        while copied < space {
            let Some(byte) = self.pending.pop_front() else {
                break;
            };
            self.buf[self.buf_end + copied] = byte;
            copied += 1;
        }
        self.buf_end += copied;
        copied > 0
    }

    fn skip_paste_remainder(&mut self) -> SkipPasteResult {
        const PASTE_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

        loop {
            if self.buf_pos >= self.buf_end {
                if self.shift_pending_into_buf() {
                    continue;
                }
                match self.poll(100) {
                    PollResult::Stdin => {
                        if self.fill_buf().is_none() {
                            self.in_paste = false;
                            return SkipPasteResult::Complete;
                        }
                    }
                    PollResult::Signal => return SkipPasteResult::Signal,
                    PollResult::Timeout => continue,
                    PollResult::StdinHup => {
                        self.in_paste = false;
                        return SkipPasteResult::Complete;
                    }
                }
            } else {
                let remaining = &self.buf[self.buf_pos..self.buf_end];

                if let Some(esc_off) = remaining.iter().position(|&b| b == 0x1b) {
                    let after_esc = &remaining[esc_off..];
                    if after_esc.len() >= PASTE_END.len()
                        && after_esc[..PASTE_END.len()] == *PASTE_END
                    {
                        self.buf_pos += esc_off + PASTE_END.len();
                        self.in_paste = false;
                        return SkipPasteResult::Complete;
                    }
                    if after_esc.len() < PASTE_END.len() {
                        let partial_len = after_esc.len();
                        self.buf_pos += esc_off;
                        self.buf.copy_within(self.buf_pos..self.buf_end, 0);
                        self.buf_pos = 0;
                        self.buf_end = partial_len;
                        match self.poll(100) {
                            PollResult::Signal => return SkipPasteResult::Signal,
                            PollResult::Timeout => continue,
                            PollResult::StdinHup => {
                                self.in_paste = false;
                                return SkipPasteResult::Complete;
                            }
                            PollResult::Stdin => {}
                        }
                        if self.shift_pending_into_buf() {
                            continue;
                        }
                        // SAFETY: `stdin_fd` is borrowed and remains open for this reader's lifetime.
                        let stdin = unsafe { BorrowedFd::borrow_raw(self.stdin_fd) };
                        match rustix::io::read(stdin, &mut self.buf[self.buf_end..]) {
                            Ok(0) => {
                                self.in_paste = false;
                                return SkipPasteResult::Complete;
                            }
                            Ok(n) => {
                                self.buf_end += n;
                                continue;
                            }
                            Err(_) => {
                                self.in_paste = false;
                                return SkipPasteResult::Complete;
                            }
                        }
                    }
                    self.buf_pos += esc_off + 1;
                } else {
                    self.buf_pos = self.buf_end;
                }
            }
        }
    }

    fn has_buffered(&self) -> bool {
        self.buf_pos < self.buf_end || !self.pending.is_empty()
    }

    /// Consume buffered bytes until a key completes. `None` means the buffered
    /// bytes ran out, a paste marker was consumed, or the read failed; a
    /// sequence still in flight keeps its parser state for the next call.
    /// The caller guarantees buffered bytes or a readable terminal.
    fn decode_key(&mut self) -> Option<KeyEvent> {
        let mut first = true;
        loop {
            let byte = if first || self.has_buffered() {
                first = false;
                self.read_byte()?
            } else if self.seq == Seq::Escape {
                if !self.poll_stdin(ESCAPE_TIMEOUT_MS) {
                    self.seq = Seq::Ground;
                    return Some(KeyEvent::key(Key::Escape));
                }
                self.read_byte()?
            } else {
                return None;
            };
            match self.feed(byte) {
                Fed::Key(key) => return Some(key),
                Fed::Boundary => return None,
                Fed::Pending => {}
            }
        }
    }

    fn feed(&mut self, byte: u8) -> Fed {
        match self.seq {
            Seq::Ground => self.feed_ground(byte),
            Seq::Escape => self.feed_escape(byte),
            Seq::Csi | Seq::Ss3 => self.feed_params(byte),
            Seq::Str { esc } => self.feed_string(byte, esc),
            Seq::Mouse { left } => {
                self.seq = if left > 1 { Seq::Mouse { left: left - 1 } } else { Seq::Ground };
                Fed::Pending
            }
            Seq::Utf8 { left, cp } => self.feed_utf8(byte, left, cp),
        }
    }

    fn feed_ground(&mut self, byte: u8) -> Fed {
        let key = match byte {
            0x08 => KeyEvent::with_mods(
                Key::Backspace,
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            ),
            0x09 => KeyEvent::key(Key::Tab),
            0x0a | 0x0d => KeyEvent::key(Key::Enter),
            0x1b => {
                self.seq = Seq::Escape;
                return Fed::Pending;
            }
            0x1c => KeyEvent::ctrl('\\'),
            0x1d => KeyEvent::ctrl(']'),
            0x1e => KeyEvent::ctrl('^'),
            0x1f => KeyEvent::ctrl('_'),
            0x7f => KeyEvent::key(Key::Backspace),
            b @ 0x01..=0x1a => KeyEvent::ctrl((b - 1 + b'a') as char),
            b if b < 0x20 => return Fed::Pending,
            b if b < 0x80 => KeyEvent::char(b as char),
            b => {
                let (left, cp) = if b & 0xE0 == 0xC0 {
                    (1, (b & 0x1F) as u32)
                } else if b & 0xF0 == 0xE0 {
                    (2, (b & 0x0F) as u32)
                } else if b & 0xF8 == 0xF0 {
                    (3, (b & 0x07) as u32)
                } else {
                    return Fed::Pending;
                };
                self.seq = Seq::Utf8 { left, cp };
                return Fed::Pending;
            }
        };
        Fed::Key(key)
    }

    fn feed_utf8(&mut self, byte: u8, left: u8, cp: u32) -> Fed {
        if byte & 0xC0 != 0x80 {
            self.seq = Seq::Ground;
            return self.feed_ground(byte);
        }
        let cp = (cp << 6) | (byte & 0x3F) as u32;
        if left > 1 {
            self.seq = Seq::Utf8 { left: left - 1, cp };
            return Fed::Pending;
        }
        self.seq = Seq::Ground;
        match char::from_u32(cp) {
            Some(c) => Fed::Key(KeyEvent::char(c)),
            None => Fed::Pending,
        }
    }

    fn feed_escape(&mut self, byte: u8) -> Fed {
        self.seq = Seq::Ground;
        match byte {
            b'[' => {
                self.csi = Csi::EMPTY;
                self.seq = Seq::Csi;
                Fed::Pending
            }
            b'O' => {
                self.csi = Csi::EMPTY;
                self.seq = Seq::Ss3;
                Fed::Pending
            }
            b']' | b'P' | b'_' | b'^' | b'X' => {
                self.seq = Seq::Str { esc: false };
                Fed::Pending
            }
            b if (0x20..0x7f).contains(&b) => Fed::Key(KeyEvent::alt(b as char)),
            _ => Fed::Key(KeyEvent::key(Key::Escape)),
        }
    }

    fn feed_string(&mut self, byte: u8, esc: bool) -> Fed {
        if esc {
            return match byte {
                b'\\' => {
                    self.seq = Seq::Ground;
                    Fed::Pending
                }
                0x1b => Fed::Pending,
                _ => {
                    self.seq = Seq::Ground;
                    self.feed_escape(byte)
                }
            };
        }
        match byte {
            0x07 | 0x18 | 0x1a => self.seq = Seq::Ground,
            0x03 => {
                self.seq = Seq::Ground;
                return self.feed_ground(byte);
            }
            0x1b => self.seq = Seq::Str { esc: true },
            _ => {}
        }
        Fed::Pending
    }

    fn feed_params(&mut self, byte: u8) -> Fed {
        let csi = &mut self.csi;
        match byte {
            b'0'..=b'9' => {
                csi.digit(byte);
                return Fed::Pending;
            }
            b';' => {
                csi.separator();
                return Fed::Pending;
            }
            b':' => {
                csi.started = true;
                csi.in_sub = true;
                csi.unsupported = true;
                return Fed::Pending;
            }
            0x3c..=0x3f => {
                if csi.started {
                    csi.unsupported = true;
                } else {
                    csi.private = true;
                }
                csi.started = true;
                return Fed::Pending;
            }
            0x20..=0x2f => {
                csi.started = true;
                csi.unsupported = true;
                return Fed::Pending;
            }
            0x40..=0x7e => {}
            0x18 | 0x1a => {
                self.seq = Seq::Ground;
                return Fed::Pending;
            }
            0x1b => {
                self.seq = Seq::Escape;
                return Fed::Pending;
            }
            _ => {
                self.seq = Seq::Ground;
                return self.feed_ground(byte);
            }
        }
        csi.finish();
        let ss3 = self.seq == Seq::Ss3;
        self.seq = Seq::Ground;
        let csi = self.csi;
        if csi.unsupported || csi.private {
            return Fed::Pending;
        }
        if ss3 {
            return match self.ss3_to_key(byte, csi.values()) {
                Some(key) => Fed::Key(key),
                None => Fed::Pending,
            };
        }
        if byte == b'M' && csi.count == 0 {
            self.seq = Seq::Mouse { left: X10_MOUSE_BYTES };
            return Fed::Pending;
        }
        let was_paste = self.in_paste;
        match self.csi_to_key(byte, csi.values()) {
            Some(key) => Fed::Key(key),
            None if self.in_paste != was_paste => Fed::Boundary,
            None => Fed::Pending,
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

    fn ss3_to_key(&self, final_byte: u8, params: &[u32]) -> Option<KeyEvent> {
        let mods = params.first().map_or(Modifiers::NONE, |&p| modifier_from_param(p));
        let key = match final_byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            _ => return None,
        };
        Some(KeyEvent::with_mods(key, mods))
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
        let m = modifier_from_param(5);
        assert!(m.ctrl);
        assert!(!m.alt);
        assert!(!m.shift);

        let m = modifier_from_param(3);
        assert!(!m.ctrl);
        assert!(m.alt);
        assert!(!m.shift);
    }

    #[test]
    fn transfer_preserves_buffer_before_later_pending_input() {
        let mut reader = InputReader::new(-1);
        reader.buf[..3].copy_from_slice(b"old");
        reader.buf_end = 3;
        reader.push_pending(b"new");
        assert_eq!(reader.take_pending_bytes(), b"oldnew");
    }

    #[test]
    fn eof_on_stdin_marks_the_terminal_dead() {
        let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
        drop(stdin_write);
        let (signal_read, _signal_write) = rustix::pipe::pipe().unwrap();
        let mut reader =
            InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
        assert!(reader.fill_buf().is_none());
        assert!(reader.stdin_dead);
        assert!(matches!(
            reader.poll_fds(stdin_read.as_raw_fd(), -1),
            PollResult::StdinHup
        ));
        assert!(!reader.poll_stdin(0));
    }

    #[test]
    fn hung_up_terminal_reads_as_eof_without_spinning() {
        let master = crate::capture::openpt_cloexec().expect("pty master");
        rustix::pty::grantpt(&master).expect("grantpt");
        rustix::pty::unlockpt(&master).expect("unlockpt");
        let slave_name = rustix::pty::ptsname(&master, Vec::new()).expect("slave name");
        let slave = rustix::fs::open(
            slave_name.as_c_str(),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .expect("slave");
        drop(master);
        let (signal_read, signal_write) = rustix::pipe::pipe().expect("signal pipe");
        let slave = Box::leak(Box::new(slave));
        let signal_read = Box::leak(Box::new(signal_read));
        let _signal_write = Box::leak(Box::new(signal_write));
        let (done, waited) = std::sync::mpsc::sync_channel(1);
        // Detached: a regression spins here forever, so the test must not
        // join this thread to report the failure.
        std::thread::spawn(move || {
            let mut reader =
                InputReader::with_fds(slave.as_raw_fd(), signal_read.as_raw_fd());
            let _ = done.send(reader.read_event());
        });
        match waited.recv_timeout(Duration::from_secs(10)) {
            Ok(InputEvent::Key(key)) => assert!(
                key.key == Key::Char('d') && key.mods.ctrl,
                "hung-up terminal must read as Ctrl+D, got {key:?}"
            ),
            Ok(InputEvent::Signal(_)) => panic!("hung-up terminal read as a signal"),
            Ok(InputEvent::Paste(_)) => panic!("hung-up terminal read as a paste"),
            Ok(InputEvent::PasteRejected) => panic!("hung-up terminal read as a rejected paste"),
            Err(_) => panic!("read_event spun on a hung-up terminal instead of returning"),
        }
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
    fn oversized_csi_parameter_is_discarded_through_its_final_byte() {
        let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&stdin_write, b"\x1b[4294967296Ax").unwrap();
        let mut reader = InputReader::with_fds(stdin_read.as_raw_fd(), -1);

        assert_eq!(reader.decode_key().unwrap().key, Key::Char('x'));
    }

    #[test]
    fn paste_start_end_resets_state() {
        let mut reader = InputReader::new(-1);

        assert!(!reader.in_paste());

        assert!(reader.csi_to_key(b'~', &[200]).is_none());
        assert!(reader.in_paste());
        assert!(reader.paste_buf.is_empty());
        assert!(!reader.paste_limit_hit);

        assert!(reader.csi_to_key(b'~', &[201]).is_none());
        assert!(!reader.in_paste());
        assert!(!reader.paste_limit_hit);
    }

    #[test]
    fn paste_accumulation_stops_at_limit() {
        let mut reader = InputReader::new(-1);

        reader.csi_to_key(b'~', &[200]);
        assert!(reader.in_paste());

        for c in "a".repeat(PASTE_LIMIT).chars() {
            reader.accumulate_paste_key(KeyEvent::char(c));
        }
        assert_eq!(reader.paste_buf.len(), PASTE_LIMIT);
        assert!(!reader.paste_limit_hit);

        reader.accumulate_paste_key(KeyEvent::char('x'));
        assert!(reader.paste_buf.is_empty());
        assert!(reader.paste_limit_hit);

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

    #[test]
    fn oversized_paste_drain_returns_when_signal_pipe_is_ready() {
        let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, signal_write) = rustix::pipe::pipe().unwrap();
        let mut reader = InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
        reader.in_paste = true;
        reader.paste_limit_hit = true;
        let (send_result, receive_result) = std::sync::mpsc::sync_channel(1);
        let signal_sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            rustix::io::write(&signal_write, b"x").unwrap();
        });
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let result = reader.skip_paste_remainder();
            send_result.send(()).unwrap();
            result
        });
        let observed = receive_result.recv_timeout(Duration::from_secs(1));
        drop(stdin_write);
        let returned = worker.join().unwrap();
        signal_sender.join().unwrap();

        observed.expect("paste drain should observe the signal");
        assert!(matches!(returned, SkipPasteResult::Signal));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[derive(Debug, PartialEq)]
    enum Seen {
        Key(Key, bool, bool, bool),
        Paste(String),
    }

    fn key(key: Key) -> Seen {
        Seen::Key(key, false, false, false)
    }

    fn ch(c: char) -> Seen {
        key(Key::Char(c))
    }

    fn mod_key(key: Key, ctrl: bool, alt: bool, shift: bool) -> Seen {
        Seen::Key(key, ctrl, alt, shift)
    }

    /// Write each part to a reader's terminal after `gap`, then a sentinel
    /// `Z`, and return every event decoded before the sentinel.
    fn decode_parts(parts: &[&[u8]], gap: Duration) -> Vec<Seen> {
        let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, signal_write) = rustix::pipe::pipe().unwrap();
        let parts: Vec<Vec<u8>> = parts.iter().map(|part| part.to_vec()).collect();
        let writer = std::thread::spawn(move || {
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    std::thread::sleep(gap);
                }
                let mut rest = part.as_slice();
                while !rest.is_empty() {
                    let n = rustix::io::write(&stdin_write, rest).unwrap();
                    rest = &rest[n..];
                }
            }
            std::thread::sleep(gap);
            rustix::io::write(&stdin_write, b"Z").unwrap();
            stdin_write
        });
        let (done, observed) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader =
                InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
            let mut seen = Vec::new();
            loop {
                match reader.read_event() {
                    InputEvent::Key(event) => {
                        if event.key == Key::Char('Z') {
                            break;
                        }
                        seen.push(Seen::Key(
                            event.key,
                            event.mods.ctrl,
                            event.mods.alt,
                            event.mods.shift,
                        ));
                    }
                    InputEvent::Paste(text) => seen.push(Seen::Paste(text)),
                    InputEvent::PasteRejected | InputEvent::Signal(_) => panic!("unexpected event"),
                }
            }
            let _ = done.send(seen);
            drop(signal_write);
        });
        let seen = observed
            .recv_timeout(Duration::from_secs(20))
            .expect("decoder must reach the sentinel instead of stalling");
        let _ = writer.join();
        seen
    }

    fn assert_framed_at_every_boundary(name: &str, sequence: &[u8], expected: fn() -> Vec<Seen>) {
        let whole = decode_parts(&[sequence], Duration::from_millis(0));
        assert_eq!(whole, expected(), "{name}: unsplit");
        // An ESC that opens a sequence and then stays alone past the timeout
        // is the Escape key by design, so any real pause right after it races
        // that timeout on a loaded machine; the unsplit case covers it. An ESC
        // inside a string is the start of its terminator and may be split.
        let failures: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (1..sequence.len())
                .flat_map(|at| [(at, 10u64), (at, 80u64)])
                .filter(|&(at, _)| !(sequence[at - 1] == 0x1b && sequence[at] != b'\\'))
                .map(|(at, gap)| {
                    scope.spawn(move || {
                        let parts = [&sequence[..at], &sequence[at..]];
                        let got = decode_parts(&parts, Duration::from_millis(gap));
                        (got != expected()).then(|| {
                            format!("{name}: split at {at}, gap {gap}ms: {got:?}")
                        })
                    })
                })
                .collect();
            workers.into_iter().filter_map(|worker| worker.join().unwrap()).collect()
        });
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn reply_and_report_sequences_are_consumed_whole_at_every_split() {
        let ignored: &[(&str, &[u8])] = &[
            ("primary device attributes", b"\x1b[?1;2c"),
            ("secondary device attributes", b"\x1b[>0;276;0c"),
            ("tertiary device attributes", b"\x1b[=0c"),
            ("cursor position report", b"\x1b[12;40R"),
            ("mode report with intermediate", b"\x1b[?2004;1$y"),
            ("intermediate before final", b"\x1b[0 q"),
            ("sgr mouse press", b"\x1b[<0;12;8M"),
            ("sgr mouse release", b"\x1b[<0;120;80m"),
            ("sgr mouse wheel", b"\x1b[<64;1;1M"),
            ("urxvt mouse", b"\x1b[32;12;8M"),
            ("x10 mouse", b"\x1b[M #!"),
            ("focus in", b"\x1b[I"),
            ("focus out", b"\x1b[O"),
            ("stray paste end", b"\x1b[201~"),
            ("csi-u", b"\x1b[97;5u"),
            ("csi-u with subparameters", b"\x1b[97:65;5:3u"),
            ("csi-u with text subparameters", b"\x1b[97;2;65:66u"),
            ("modified arrow with event type", b"\x1b[1;5:3A"),
            ("unsupported function key", b"\x1b[15~"),
            ("unsupported ss3 key", b"\x1bOP"),
            ("ss3 key with parameters", b"\x1bO2P"),
            ("osc bel", b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07"),
            ("osc st", b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"),
            ("osc clipboard", b"\x1b]52;c;SGVsbG8=\x1b\\"),
            ("osc with printable text", b"\x1b]0;a b [c] d\x07"),
            ("dcs", b"\x1bP>|terminal 1.0\x1b\\"),
            ("dcs request reply", b"\x1bP1$r0m\x1b\\"),
            ("apc", b"\x1b_Gi=31;OK\x1b\\"),
            ("pm", b"\x1b^private\x1b\\"),
            ("sos", b"\x1bXstring\x1b\\"),
        ];
        for (name, sequence) in ignored {
            assert_framed_at_every_boundary(name, sequence, Vec::new);
        }
    }

    #[test]
    fn supported_keys_still_decode_at_every_split() {
        type KeyCase = (&'static str, &'static [u8], fn() -> Vec<Seen>);
        let keys: &[KeyCase] = &[
            ("up", b"\x1b[A", || vec![key(Key::Up)]),
            ("ctrl right", b"\x1b[1;5C", || vec![mod_key(Key::Right, true, false, false)]),
            ("alt left", b"\x1b[1;3D", || vec![mod_key(Key::Left, false, true, false)]),
            ("home", b"\x1b[H", || vec![key(Key::Home)]),
            ("end tilde", b"\x1b[4~", || vec![key(Key::End)]),
            ("delete", b"\x1b[3~", || vec![key(Key::Delete)]),
            ("ctrl delete", b"\x1b[3;5~", || vec![mod_key(Key::Delete, true, false, false)]),
            ("shift tab", b"\x1b[Z", || vec![mod_key(Key::Tab, false, false, true)]),
            ("ss3 home", b"\x1bOH", || vec![key(Key::Home)]),
            ("ss3 shift up", b"\x1bO2A", || vec![mod_key(Key::Up, false, false, true)]),
            ("alt b", b"\x1bb", || vec![mod_key(Key::Char('b'), false, true, false)]),
            ("two byte char", "\u{e9}".as_bytes(), || vec![ch('\u{e9}')]),
            ("three byte char", "\u{20ac}".as_bytes(), || vec![ch('\u{20ac}')]),
            ("four byte char", "\u{1f600}".as_bytes(), || vec![ch('\u{1f600}')]),
            ("bracketed paste", b"\x1b[200~ab\ncd\x1b[201~", || vec![Seen::Paste("ab\ncd".into())]),
        ];
        for (name, sequence, expected) in keys {
            assert_framed_at_every_boundary(name, sequence, *expected);
        }
    }

    #[test]
    fn replies_between_typed_keys_leave_the_typing_intact() {
        let stream = b"a\x1b[?1;2c\x1b[Ib\x1b]11;rgb:0/0/0\x07c\x1b[<0;1;1Md\x1bP>|x\x1b\\e";
        for gap in [0u64, 70] {
            let mut parts: Vec<&[u8]> = Vec::new();
            let mut start = 0;
            for end in 1..=stream.len() {
                let after_escape = stream[end - 1] == 0x1b;
                if end == stream.len() || (end - start >= 3 && !after_escape) {
                    parts.push(&stream[start..end]);
                    start = end;
                }
            }
            let got = decode_parts(&parts, Duration::from_millis(gap));
            assert_eq!(got, vec![ch('a'), ch('b'), ch('c'), ch('d'), ch('e')], "gap {gap}ms");
        }
    }

    #[test]
    fn lone_escape_after_the_timeout_is_still_the_escape_key() {
        let got = decode_parts(&[b"\x1b", b"x"], Duration::from_millis(120));
        assert_eq!(got, vec![key(Key::Escape), ch('x')]);
    }

    #[test]
    fn escape_inside_a_string_or_a_cancel_byte_ends_the_sequence() {
        let got = decode_parts(&[b"\x1b]0;title\x1b[A"], Duration::from_millis(0));
        assert_eq!(got, vec![key(Key::Up)]);
        let got = decode_parts(&[b"\x1b]0;title\x18a"], Duration::from_millis(0));
        assert_eq!(got, vec![ch('a')]);
        let got = decode_parts(&[b"\x1b]0;unterminated\x03"], Duration::from_millis(0));
        assert_eq!(got, vec![Seen::Key(Key::Char('c'), true, false, false)]);
        let got = decode_parts(&[b"\x1b[1\r"], Duration::from_millis(0));
        assert_eq!(got, vec![key(Key::Enter)]);
        let got = decode_parts(&[b"\x1b[1\x1b[B"], Duration::from_millis(0));
        assert_eq!(got, vec![key(Key::Down)]);
    }

    #[test]
    fn oversized_sequences_are_discarded_whole() {
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend(std::iter::repeat_n(b'A', 100_000));
        osc.push(0x07);
        let got = decode_parts(&[&osc], Duration::from_millis(0));
        assert!(got.is_empty(), "{} events leaked", got.len());

        let mut csi = b"\x1b[".to_vec();
        csi.extend(std::iter::repeat_n(b'1', 5_000));
        csi.extend_from_slice(b";2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20A");
        let got = decode_parts(&[&csi], Duration::from_millis(0));
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn partial_reply_returned_as_typeahead_is_framed_not_inserted() {
        let (stdin_read, _stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, _signal_write) = rustix::pipe::pipe().unwrap();
        let mut reader = InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
        reader.push_pending(b"\x1b[?6");
        assert!(reader.decode_key().is_none());
        reader.push_pending(b"4;1c\x1b]11;rgb:");
        assert!(reader.decode_key().is_none());
        reader.push_pending(b"0000/0000/0000\x07Z");
        assert_eq!(reader.decode_key().unwrap().key, Key::Char('Z'));
        assert!(!reader.has_buffered());
    }

    #[test]
    fn buffered_bytes_after_a_consumed_sequence_are_not_stranded() {
        let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, _signal_write) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&stdin_write, b"\x1b[I\x1b[201~\x1b[?1;2cx").unwrap();
        let mut reader = InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
        match reader.read_event() {
            InputEvent::Key(event) => assert_eq!(event.key, Key::Char('x')),
            _ => panic!("expected the typed key"),
        }
    }

    #[test]
    fn taking_pending_bytes_drops_a_sequence_in_flight() {
        let (stdin_read, _stdin_write) = rustix::pipe::pipe().unwrap();
        let (signal_read, _signal_write) = rustix::pipe::pipe().unwrap();
        let mut reader = InputReader::with_fds(stdin_read.as_raw_fd(), signal_read.as_raw_fd());
        reader.push_pending(b"\x1b[?1");
        assert!(reader.decode_key().is_none());
        assert!(reader.take_pending_bytes().is_empty());
        reader.push_pending(b"x");
        assert_eq!(reader.decode_key().unwrap().key, Key::Char('x'));
    }
}
