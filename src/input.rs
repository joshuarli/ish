use crate::signal;
use crate::term::STDIN_FD;
use std::os::fd::RawFd;

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
}

pub struct InputReader {
    signal_fd: RawFd,
}

enum PollResult {
    Stdin,
    Signal,
    Timeout,
    Error,
}

impl InputReader {
    pub fn new(signal_fd: RawFd) -> Self {
        Self { signal_fd }
    }

    /// Block until a key or signal event is available.
    pub fn read_event(&mut self) -> InputEvent {
        loop {
            match self.poll(-1) {
                PollResult::Stdin => {
                    if let Some(key) = self.decode_key() {
                        return InputEvent::Key(key);
                    }
                }
                PollResult::Signal => {
                    if let Some(sig) = signal::read_signal() {
                        return InputEvent::Signal(sig);
                    }
                }
                PollResult::Timeout | PollResult::Error => {}
            }
        }
    }

    fn poll(&self, timeout_ms: i32) -> PollResult {
        let mut fds = [
            libc::pollfd {
                fd: STDIN_FD,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.signal_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return PollResult::Error;
            }
            if n == 0 {
                return PollResult::Timeout;
            }
            if fds[1].revents & libc::POLLIN != 0 {
                return PollResult::Signal;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                return PollResult::Stdin;
            }
            return PollResult::Error;
        }
    }

    fn poll_stdin(&self, timeout_ms: i32) -> bool {
        let mut fds = [libc::pollfd {
            fd: STDIN_FD,
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        n > 0 && fds[0].revents & libc::POLLIN != 0
    }

    fn read_byte(&self) -> Option<u8> {
        let mut byte = 0u8;
        let n = unsafe { libc::read(STDIN_FD, &mut byte as *mut u8 as *mut libc::c_void, 1) };
        if n == 1 { Some(byte) } else { None }
    }

    fn read_byte_timeout(&self, timeout_ms: i32) -> Option<u8> {
        if self.poll_stdin(timeout_ms) {
            self.read_byte()
        } else {
            None
        }
    }

    fn decode_key(&mut self) -> Option<KeyEvent> {
        let byte = self.read_byte()?;

        match byte {
            0x00 => None,
            0x01 => Some(KeyEvent::ctrl('a')),
            0x02 => Some(KeyEvent::ctrl('b')),
            0x03 => Some(KeyEvent::ctrl('c')),
            0x04 => Some(KeyEvent::ctrl('d')),
            0x05 => Some(KeyEvent::ctrl('e')),
            0x06 => Some(KeyEvent::ctrl('f')),
            0x08 => Some(KeyEvent::key(Key::Backspace)),
            0x09 => Some(KeyEvent::key(Key::Tab)),
            0x0a | 0x0d => Some(KeyEvent::key(Key::Enter)),
            0x0b => Some(KeyEvent::ctrl('k')),
            0x0c => Some(KeyEvent::ctrl('l')),
            0x0e => Some(KeyEvent::ctrl('n')),
            0x10 => Some(KeyEvent::ctrl('p')),
            0x12 => Some(KeyEvent::ctrl('r')),
            0x15 => Some(KeyEvent::ctrl('u')),
            0x17 => Some(KeyEvent::ctrl('w')),
            0x19 => Some(KeyEvent::ctrl('y')),
            0x1a => Some(KeyEvent::ctrl('z')),
            0x1b => self.decode_escape(),
            0x7f => Some(KeyEvent::key(Key::Backspace)),
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

    fn csi_to_key(&self, final_byte: u8, params: &[u32]) -> Option<KeyEvent> {
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
}
