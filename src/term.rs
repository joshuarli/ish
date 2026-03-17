use std::io::{self, Write};
use std::os::fd::RawFd;


pub const STDIN_FD: RawFd = 0;
pub const STDOUT_FD: RawFd = 1;

pub struct RawMode {
    orig: libc::termios,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let orig = unsafe {
            let mut t = std::mem::zeroed();
            if libc::tcgetattr(STDIN_FD, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            t
        };

        let mut raw = orig;
        // Disable: echo, canonical mode, extended input processing, signal generation
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        // Disable: XON/XOFF flow control, CR-to-NL
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        // Keep OPOST enabled so \n → \r\n
        // 8-bit chars
        raw.c_cflag |= libc::CS8;
        // Return immediately from read, no minimum chars
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;

        unsafe {
            if libc::tcsetattr(STDIN_FD, libc::TCSAFLUSH, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(Self { orig })
    }

    pub fn original(&self) -> &libc::termios {
        &self.orig
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(STDIN_FD, libc::TCSAFLUSH, &self.orig);
        }
    }
}

pub fn term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(STDOUT_FD, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}

/// Save original termios without entering raw mode (for child process restoration).
pub fn save_termios() -> io::Result<libc::termios> {
    unsafe {
        let mut t = std::mem::zeroed();
        if libc::tcgetattr(STDIN_FD, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(t)
    }
}

pub struct TermWriter {
    buf: Vec<u8>,
}

impl TermWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(2048),
        }
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

    pub fn write_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn clear_screen(&mut self) {
        // ESC[H moves cursor home, ESC[2J clears visible screen (not scrollback)
        self.write_str("\x1b[H\x1b[2J");
    }

    pub fn cursor_to(&mut self, row: u16, col: u16) {

        self.write_str(&format!("\x1b[{};{}H", row + 1, col + 1));
    }

    pub fn clear_to_end_of_screen(&mut self) {
        self.write_str("\x1b[J");
    }

    pub fn clear_to_end_of_line(&mut self) {
        self.write_str("\x1b[K");
    }

    pub fn clear_line(&mut self) {
        self.write_str("\x1b[2K");
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

    pub fn newline(&mut self) {
        self.write_str("\n");
    }

    pub fn move_cursor_right(&mut self, n: u16) {
        if n > 0 {
    
            self.write_str(&format!("\x1b[{}C", n));
        }
    }

    pub fn move_cursor_up(&mut self, n: u16) {
        if n > 0 {
    
            self.write_str(&format!("\x1b[{}A", n));
        }
    }

    pub fn move_cursor_down(&mut self, n: u16) {
        if n > 0 {
    
            self.write_str(&format!("\x1b[{}B", n));
        }
    }

    /// Set foreground color by ANSI 256-color index.
    pub fn set_fg(&mut self, color: u8) {

        self.write_str(&format!("\x1b[38;5;{}m", color));
    }

    pub fn set_bold(&mut self) {
        self.write_str("\x1b[1m");
    }

    pub fn reset_style(&mut self) {
        self.write_str("\x1b[0m");
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
