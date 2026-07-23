use std::io::{self, Write};
use std::os::fd::{BorrowedFd, RawFd};

pub const STDIN_FD: RawFd = 0;
pub const STDOUT_FD: RawFd = 1;

pub type Termios = rustix::termios::Termios;

pub struct RawMode {
    orig: Termios,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
        let orig = rustix::termios::tcgetattr(stdin)?;

        let mut raw = orig.clone();
        raw.make_raw();
        raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;

        // SAFETY: tcsetattr with TCSADRAIN drains output, then applies settings.
        // Preserves pending input so typeahead typed during child execution
        // is available to read_line.
        rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Drain, &raw)?;

        // Enable bracketed paste mode so the terminal wraps pastes in
        // \x1b[200~ ... \x1b[201~, letting us distinguish paste from typing.
        let _ = io::stdout().write_all(b"\x1b[?2004h");
        let _ = io::stdout().flush();

        Ok(Self { orig })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Disable bracketed paste mode before restoring cooked terminal.
        let _ = io::stdout().write_all(b"\x1b[?2004l");
        let _ = io::stdout().flush();
        // SAFETY: Restores saved termios. TCSANOW avoids blocking if output
        // hasn't drained (e.g. PTY). orig was captured in enable().
        let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
        let _ =
            rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Now, &self.orig);
    }
}

pub fn term_size() -> (u16, u16) {
    let stdout = unsafe { BorrowedFd::borrow_raw(STDOUT_FD) };
    match rustix::termios::tcgetwinsize(stdout) {
        Ok(ws) if ws.ws_col > 0 => (ws.ws_row, ws.ws_col),
        _ => (24, 80),
    }
}

/// Save original termios without entering raw mode (for child process restoration).
pub fn save_termios() -> io::Result<Termios> {
    let stdin = unsafe { BorrowedFd::borrow_raw(STDIN_FD) };
    Ok(rustix::termios::tcgetattr(stdin)?)
}

pub struct TermWriter {
    buf: Vec<u8>,
}

impl Default for TermWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl TermWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(2048),
        }
    }

    pub fn clear_buffer(&mut self) {
        self.buf.clear();
    }

    pub fn buffer_len(&self) -> usize {
        self.buf.len()
    }

    pub fn flush_to_stdout(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&self.buf)?;
            stdout.flush()?;
            self.buf.clear();
        }
        Ok(())
    }

    pub fn write_str(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn clear_screen(&mut self) {
        // ESC[H moves cursor home, ESC[2J clears visible screen (not scrollback)
        self.write_str("\x1b[H\x1b[2J");
    }

    pub fn clear_to_end_of_screen(&mut self) {
        self.write_str("\x1b[J");
    }

    pub fn clear_to_end_of_line(&mut self) {
        self.write_str("\x1b[K");
    }

    pub fn hide_cursor(&mut self) {
        self.write_str("\x1b[?25l");
    }

    pub fn show_cursor(&mut self) {
        self.write_str("\x1b[?25h");
    }

    pub fn carriage_return(&mut self) {
        self.write_str("\r");
    }

    pub fn move_cursor_right(&mut self, n: u16) {
        if n > 0 {
            self.push_csi(n, b'C');
        }
    }

    pub fn move_cursor_up(&mut self, n: u16) {
        if n > 0 {
            self.push_csi(n, b'A');
        }
    }

    pub fn move_cursor_down(&mut self, n: u16) {
        if n > 0 {
            self.push_csi(n, b'B');
        }
    }

    pub fn save_cursor(&mut self) {
        self.write_str("\x1b7");
    }

    pub fn restore_cursor(&mut self) {
        self.write_str("\x1b8");
    }

    /// Write CSI sequence `\x1b[{n}{suffix}` directly into the buffer.
    /// Inline u16→ASCII avoids format!() heap allocation on every cursor move.
    fn push_csi(&mut self, n: u16, suffix: u8) {
        self.buf.extend_from_slice(b"\x1b[");
        let mut tmp = [0u8; 5];
        let mut i = tmp.len();
        let mut val = n;
        loop {
            i -= 1;
            tmp[i] = b'0' + (val % 10) as u8;
            val /= 10;
            if val == 0 {
                break;
            }
        }
        self.buf.extend_from_slice(&tmp[i..]);
        self.buf.push(suffix);
    }

    #[cfg(test)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

impl Write for TermWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_to_stdout()
    }
}
