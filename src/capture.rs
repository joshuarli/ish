//! Output capture for the most recent command.
//!
//! While a command runs, the shell's stdout and stderr are replaced by the
//! slave side of a pseudo-terminal so children keep seeing terminals. A relay
//! thread copies everything written there to the real terminal as it arrives
//! and keeps the tail in an [`OutputBuffer`]. Stdin and the controlling
//! terminal stay with the real terminal, so job control and input are
//! unchanged.
//!
//! Only bytes written to the shell's terminal are captured. A redirection or
//! pipe that takes a stream away from the terminal also takes it away from the
//! capture, and a stderr that was never the terminal is left alone.

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, fcntl_setfl, open};
use rustix::io::Errno;
use rustix::pty::{grantpt, ptsname, unlockpt};
#[cfg(not(target_os = "macos"))]
use rustix::pty::{OpenptFlags, openpt};
use rustix::termios::{OptionalActions, Winsize, tcgetattr, tcgetpgrp, tcgetwinsize, tcsetattr, tcsetwinsize};
use rustix::time::Timespec;
use std::collections::VecDeque;
use std::io;
use std::thread::JoinHandle;

pub const OUTPUT_LIMIT: usize = 1 << 20;
const RELAY_CHUNK: usize = 16 * 1024;
const RESIZE_POLL: Timespec = Timespec { tv_sec: 0, tv_nsec: 200_000_000 };

/// The tail of one command's terminal output, at most [`OUTPUT_LIMIT`] bytes.
/// The allocation is kept between commands.
#[derive(Default)]
pub struct OutputBuffer {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl OutputBuffer {
    pub fn push(&mut self, chunk: &[u8]) {
        let chunk = match chunk.len().checked_sub(OUTPUT_LIMIT) {
            Some(excess) => {
                self.truncated = true;
                &chunk[excess..]
            }
            None => chunk,
        };
        let overflow = (self.bytes.len() + chunk.len()).saturating_sub(OUTPUT_LIMIT);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
        self.bytes.extend(chunk);
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.truncated = false;
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The output as text without terminal control sequences. A truncated
    /// buffer starts after its first newline so a cut sequence cannot leak.
    pub fn plain_text(&self) -> Vec<u8> {
        let skip = if self.truncated {
            self.bytes.iter().position(|&byte| byte == b'\n').map_or(0, |at| at + 1)
        } else {
            0
        };
        strip_controls(self.bytes.iter().copied().skip(skip))
    }
}

fn strip_controls(bytes: impl Iterator<Item = u8>) -> Vec<u8> {
    enum State {
        Text,
        Escape,
        Intermediate,
        Csi,
        String,
        StringEscape,
    }
    let mut out = Vec::new();
    let mut state = State::Text;
    for byte in bytes {
        state = match (state, byte) {
            (State::Text, 0x1b) => State::Escape,
            (State::Text, b'\n') => {
                if out.last() == Some(&b'\r') {
                    out.pop();
                }
                out.push(byte);
                State::Text
            }
            (State::Text, _) => {
                out.push(byte);
                State::Text
            }
            (State::Escape, b'[') => State::Csi,
            (State::Escape, b']' | b'P' | b'X' | b'^' | b'_') => State::String,
            (State::Escape, 0x20..=0x2f) => State::Intermediate,
            (State::Escape, 0x1b) => State::Escape,
            (State::Escape, _) => State::Text,
            (State::Intermediate, 0x20..=0x2f) => State::Intermediate,
            (State::Intermediate, _) => State::Text,
            (State::Csi, 0x20..=0x3f) => State::Csi,
            (State::Csi, 0x1b) => State::Escape,
            (State::Csi, _) => State::Text,
            (State::String, 0x07) => State::Text,
            (State::String, 0x1b) => State::StringEscape,
            (State::String, _) => State::String,
            (State::StringEscape, b'\\') => State::Text,
            (State::StringEscape, 0x1b) => State::StringEscape,
            (State::StringEscape, _) => State::String,
        };
    }
    out
}

struct Finished {
    buffer: OutputBuffer,
    relay_error: Option<io::Error>,
}

/// A running relay. [`Session::finish_into`] drains the pseudo-terminal and
/// stores the captured bytes. A suspended job keeps its session alive until
/// the job finishes, because the job still holds the slave side.
pub struct Session {
    stop: OwnedFd,
    thread: Option<JoinHandle<Finished>>,
}

impl Session {
    pub fn finish_into(mut self, last: &mut OutputBuffer) {
        match self.stop_and_join() {
            Some(finished) => {
                if let Some(error) = finished.relay_error {
                    eprintln!("ish: output relay failed: {error}");
                }
                *last = finished.buffer;
            }
            None => {
                eprintln!("ish: output relay thread panicked");
                last.clear();
            }
        }
    }

    fn stop_and_join(&mut self) -> Option<Finished> {
        let thread = self.thread.take()?;
        let _ = rustix::io::write(&self.stop, &[0]);
        thread.join().ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The shell's stdout and stderr, redirected to the capture terminal until
/// [`Capture::end`] or drop restores them.
pub struct Capture {
    restore: Option<Restore>,
    session: Option<Session>,
}

struct Restore {
    stdout: OwnedFd,
    stderr: Option<OwnedFd>,
}

impl Restore {
    fn apply(self) {
        use std::io::Write;
        let _ = io::stdout().flush();
        let _ = rustix::stdio::dup2_stdout(&self.stdout);
        if let Some(stderr) = &self.stderr {
            let _ = rustix::stdio::dup2_stderr(stderr);
        }
    }
}

impl Capture {
    /// Redirect the shell's terminal output through a relay. Returns `None`
    /// when stdout is not a terminal, since then nothing is shown to capture.
    pub fn begin(buffer: OutputBuffer) -> io::Result<Option<Self>> {
        let stdout = io::stdout();
        let terminal = stdout.as_fd();
        if !rustix::termios::isatty(terminal) {
            return Ok(None);
        }
        let stderr = io::stderr();
        let captures_stderr = shares_terminal(terminal, stderr.as_fd());

        let master = openpt_cloexec()?;
        grantpt(&master)?;
        unlockpt(&master)?;
        let slave_name = ptsname(&master, Vec::new())?;
        let slave = open(
            slave_name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut modes = tcgetattr(&slave)?;
        modes.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &modes)?;
        let size = tcgetwinsize(terminal).ok();
        if let Some(size) = size {
            tcsetwinsize(&slave, size)?;
        }
        fcntl_setfl(&master, OFlags::NONBLOCK)?;

        let relay_terminal = rustix::io::fcntl_dupfd_cloexec(terminal, 3)?;
        let saved_stdout = rustix::io::fcntl_dupfd_cloexec(terminal, 3)?;
        let saved_stderr = if captures_stderr {
            Some(rustix::io::fcntl_dupfd_cloexec(stderr.as_fd(), 3)?)
        } else {
            None
        };
        #[cfg(target_os = "macos")]
        let (stop_read, stop_write) = crate::sys::pipe_cloexec_owned()?;
        #[cfg(not(target_os = "macos"))]
        let (stop_read, stop_write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        let thread = std::thread::Builder::new()
            .name("ish-output-relay".into())
            .spawn(move || relay(master, stop_read, relay_terminal, size, buffer))?;
        let session = Session { stop: stop_write, thread: Some(thread) };

        use std::io::Write;
        let _ = io::stdout().flush();
        let restore = Restore { stdout: saved_stdout, stderr: saved_stderr };
        rustix::stdio::dup2_stdout(&slave)?;
        if captures_stderr
            && let Err(error) = rustix::stdio::dup2_stderr(&slave)
        {
            restore.apply();
            return Err(error.into());
        }
        Ok(Some(Self { restore: Some(restore), session: Some(session) }))
    }

    /// Give the shell its own stdout and stderr back.
    pub fn end(mut self) -> Session {
        if let Some(restore) = self.restore.take() {
            restore.apply();
        }
        self.session.take().expect("a capture holds its session until it ends")
    }
}

#[cfg(target_os = "macos")]
fn openpt_cloexec() -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;

    // macOS posix_openpt supports O_CLOEXEC, which rustix omits from its Apple flags.
    let descriptor = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: posix_openpt returned a newly allocated descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(not(target_os = "macos"))]
fn openpt_cloexec() -> io::Result<OwnedFd> {
    openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).map_err(Into::into)
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore.apply();
        }
    }
}

fn shares_terminal(first: BorrowedFd<'_>, second: BorrowedFd<'_>) -> bool {
    rustix::termios::isatty(second)
        && match (rustix::fs::fstat(first), rustix::fs::fstat(second)) {
            (Ok(first), Ok(second)) => first.st_rdev == second.st_rdev,
            _ => false,
        }
}

fn relay(
    master: OwnedFd,
    stop: OwnedFd,
    terminal: OwnedFd,
    mut applied_size: Option<Winsize>,
    buffer: OutputBuffer,
) -> Finished {
    let mut pump = Pump { master, terminal, buffer, error: None, open: true };
    let mut chunk = [0u8; RELAY_CHUNK];
    loop {
        let mut fds = [
            PollFd::new(&stop, PollFlags::IN),
            PollFd::new(&pump.master, PollFlags::IN),
        ];
        let watched = if pump.open { 2 } else { 1 };
        match poll(&mut fds[..watched], Some(&RESIZE_POLL)) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => {
                pump.error.get_or_insert(error.into());
                break;
            }
        }
        let stopping = !fds[0].revents().is_empty();
        if pump.open {
            pump.drain(&mut chunk, usize::MAX);
        }
        if stopping {
            if pump.open {
                pump.drain(&mut chunk, OUTPUT_LIMIT);
            }
            break;
        }
        follow_resize(&pump.terminal, &pump.master, &mut applied_size);
    }
    Finished { buffer: pump.buffer, relay_error: pump.error }
}

struct Pump {
    master: OwnedFd,
    terminal: OwnedFd,
    buffer: OutputBuffer,
    error: Option<io::Error>,
    open: bool,
}

impl Pump {
    fn drain(&mut self, chunk: &mut [u8], mut budget: usize) {
        while budget > 0 {
            match rustix::io::read(&self.master, &mut *chunk) {
                Ok(0) | Err(Errno::IO) => {
                    self.open = false;
                    return;
                }
                Ok(read) => {
                    budget = budget.saturating_sub(read);
                    self.buffer.push(&chunk[..read]);
                    if self.error.is_none()
                        && let Err(error) = write_terminal(&self.terminal, &chunk[..read])
                    {
                        self.error = Some(error);
                    }
                }
                Err(Errno::AGAIN) => return,
                Err(Errno::INTR) => {}
                Err(error) => {
                    self.error.get_or_insert(error.into());
                    self.open = false;
                    return;
                }
            }
        }
    }
}

fn write_terminal(terminal: &OwnedFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(terminal, bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                let mut fds = [PollFd::new(terminal, PollFlags::OUT)];
                match poll(&mut fds, None) {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn follow_resize(terminal: &OwnedFd, master: &OwnedFd, applied: &mut Option<Winsize>) {
    let Ok(current) = tcgetwinsize(terminal) else {
        return;
    };
    if *applied == Some(current) {
        return;
    }
    if tcsetwinsize(master, current).is_err() {
        return;
    }
    *applied = Some(current);
    if let Ok(foreground) = tcgetpgrp(terminal)
        && foreground != rustix::process::getpgrp()
    {
        let _ = rustix::process::kill_process_group(foreground, rustix::process::Signal::WINCH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_pty_and_stop_pipe_descriptors_are_close_on_exec() {
        let master = openpt_cloexec().expect("open close-on-exec PTY master");
        let (stop_read, stop_write) = crate::sys::pipe_cloexec_owned().expect("create close-on-exec stop pipe");
        for descriptor in [&master, &stop_read, &stop_write] {
            let flags = rustix::io::fcntl_getfd(descriptor).expect("read descriptor flags");
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
    }

    fn plain(input: &[u8]) -> Vec<u8> {
        strip_controls(input.iter().copied())
    }

    #[test]
    fn buffer_keeps_the_newest_bytes_within_the_limit() {
        let mut buffer = OutputBuffer::default();
        buffer.push(b"head\n");
        assert!(!buffer.is_truncated());
        buffer.push(&vec![b'x'; OUTPUT_LIMIT - 1]);
        assert!(buffer.is_truncated());
        assert_eq!(buffer.bytes.len(), OUTPUT_LIMIT);
        assert!(buffer.bytes.iter().rev().take(OUTPUT_LIMIT - 1).all(|&b| b == b'x'));
        assert_eq!(buffer.bytes.front(), Some(&b'\n'));
    }

    #[test]
    fn oversized_chunk_keeps_its_tail() {
        let mut buffer = OutputBuffer::default();
        let mut chunk = vec![b'a'; OUTPUT_LIMIT];
        chunk.extend_from_slice(b"tail");
        buffer.push(&chunk);
        assert_eq!(buffer.bytes.len(), OUTPUT_LIMIT);
        assert!(buffer.bytes.iter().rev().take(4).copied().eq(*b"liat"));
    }

    #[test]
    fn clear_resets_truncation_and_keeps_capacity() {
        let mut buffer = OutputBuffer::default();
        buffer.push(&vec![b'x'; OUTPUT_LIMIT + 1]);
        let capacity = buffer.bytes.capacity();
        buffer.clear();
        assert!(buffer.is_empty() && !buffer.is_truncated());
        assert_eq!(buffer.bytes.capacity(), capacity);
    }

    #[test]
    fn plain_text_drops_sequences_and_carriage_returns_before_newlines() {
        assert_eq!(plain(b"\x1b[1;31mred\x1b[0m\r\nnext\r\n"), b"red\nnext\n");
        assert_eq!(plain(b"\x1b]0;title\x07body\x1b]8;;http://x\x1b\\link"), b"bodylink");
        assert_eq!(plain(b"\x1b(Bplain\x1b7\x1b=!"), b"plain!");
        assert_eq!(plain(b"cut\x1b[3"), b"cut");
        assert_eq!(plain("héllo\n".as_bytes()), "héllo\n".as_bytes());
    }

    #[test]
    fn truncated_plain_text_starts_on_a_whole_line() {
        let mut buffer = OutputBuffer::default();
        buffer.push(b"38;5;1mred\x1b[0m\nkept\n");
        assert_eq!(buffer.plain_text(), b"38;5;1mred\nkept\n");
        buffer.truncated = true;
        assert_eq!(buffer.plain_text(), b"kept\n");
    }
}
