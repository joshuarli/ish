//! Output capture for the most recent command.
//!
//! While a command runs, the shell's standard streams that point at its
//! terminal are replaced by the slave side of a pseudo-terminal so children
//! keep seeing terminals. A relay thread copies everything written there to
//! the real terminal as it arrives and keeps the tail in an
//! [`OutputBuffer`]. Only bytes written to the shell's terminal are captured.
//!
//! A single simple command converges onto the slave: stdin, stdout, and
//! stderr all point at the slave when they pointed at the real terminal, the
//! slave starts as a copy of the real terminal's modes, and the foreground
//! child takes the slave as its controlling terminal in a session of its own.
//! The relay then forwards typed input to the slave as well, so keyboard
//! readers work no matter which descriptor they open. Bytes already read from
//! the real terminal stay with the relay until the master accepts them. A full
//! slave queue, pause, or stop does not drop them. When the session ends
//! because the child has exited, that tail is pending input for the next
//! prompt; a stopped job keeps it until forwarding resumes. The real terminal
//! sits in raw mode while the command runs and is restored when the relay stops.
//! Pipelines and lists keep the split design instead: only stdout and stderr
//! move to the slave, stdin stays on the real terminal, and the relay mirrors
//! the slave's mode changes that affect input back onto the real terminal.
//!
//! A redirection or pipe that takes a stream away from the terminal also takes
//! it away from the capture, and a stderr that was never the terminal is left
//! alone.

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, fcntl_setfl, open};
use rustix::io::Errno;
use rustix::pty::{grantpt, ptsname, unlockpt};
#[cfg(not(target_os = "macos"))]
use rustix::pty::{OpenptFlags, openpt};
use rustix::termios::{
    OptionalActions, Termios, Winsize, tcgetattr, tcgetwinsize, tcsetattr, tcsetwinsize,
};
use rustix::time::Timespec;
use std::collections::VecDeque;
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use crate::report;

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
    input_tail: Vec<u8>,
}

/// A running relay. [`Session::finish_into`] drains the pseudo-terminal and
/// stores the captured bytes. A suspended job keeps its session alive until
/// the job finishes, because the job still holds the slave side.
pub struct Session {
    stop: OwnedFd,
    thread: Option<JoinHandle<Finished>>,
    converged: bool,
    paused: Arc<AtomicBool>,
    slave_ctl: Option<OwnedFd>,
    real_raw: Option<Termios>,
}

impl Session {
    /// Whether the captured command owns the slave as its controlling
    /// terminal, with stdin, stdout, and stderr converged onto it.
    pub fn is_converged(&self) -> bool {
        self.converged
    }

    /// Pause or resume forwarding typed input to the slave. The shell pauses
    /// while a stopped job leaves the real terminal to the next prompt, so
    /// prompt keystrokes are not forwarded into the suspended child.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    /// The slave's current modes. Only meaningful for converged commands,
    /// whose child drives the slave directly.
    pub fn slave_termios(&self) -> io::Result<Termios> {
        let Some(slave) = self.slave_ctl.as_ref() else {
            return Err(io::Error::other("no converged capture terminal"));
        };
        tcgetattr(slave).map_err(std::io::Error::from)
    }

    /// Restore the slave's modes, used when resuming a stopped converged job.
    pub fn restore_slave_termios(&self, modes: &Termios) -> io::Result<()> {
        let Some(slave) = self.slave_ctl.as_ref() else {
            return Err(io::Error::other("no converged capture terminal"));
        };
        tcsetattr(slave, OptionalActions::Now, modes).map_err(std::io::Error::from)
    }

    /// The raw modes the real terminal uses while a converged command runs.
    pub fn real_raw_termios(&self) -> Option<Termios> {
        self.real_raw.clone()
    }

    /// Stops the relay and stores captured output in `last`. Returns input
    /// already read from the real terminal and not accepted by the master.
    /// The caller passes that tail to the editor only when the child has exited.
    pub fn finish_into(mut self, last: &mut OutputBuffer) -> Vec<u8> {
        match self.stop_and_join() {
            Some(finished) => {
                if let Some(error) = finished.relay_error {
                    report!("ish: output relay failed: {error}");
                }
                *last = finished.buffer;
                finished.input_tail
            }
            None => {
                report!("ish: output relay thread panicked");
                last.clear();
                Vec::new()
            }
        }
    }

    fn stop_and_join(&mut self) -> Option<Finished> {
        let thread = self.thread.take()?;
        let _ = rustix::io::write(&self.stop, &[0]);
        match thread.join() {
            Ok(finished) => Some(finished),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic payload".to_string());
                report!("ish: output relay thread panicked: {message}");
                None
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The shell's standard streams, redirected to the capture terminal until
/// [`Capture::end`] or drop restores them.
pub struct Capture {
    restore: Option<Restore>,
    session: Option<Session>,
}

struct Restore {
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: Option<OwnedFd>,
}

impl Restore {
    fn apply(self) {
        use std::io::Write;
        let _ = io::stdout().flush();
        if let Some(stdin) = &self.stdin {
            let _ = rustix::stdio::dup2_stdin(stdin);
        }
        let _ = rustix::stdio::dup2_stdout(&self.stdout);
        if let Some(stderr) = &self.stderr {
            let _ = rustix::stdio::dup2_stderr(stderr);
        }
    }
}

impl Capture {
    /// Redirect the shell's terminal streams through a relay. Returns `None`
    /// when stdout is not a terminal, since then nothing is shown to capture.
    /// When `converged` is set, stdin joins stdout and stderr on the slave,
    /// which starts as a copy of the real terminal's modes; the foreground
    /// child is expected to claim the slave as its controlling terminal.
    pub fn begin(buffer: OutputBuffer, converged: bool) -> io::Result<Option<Self>> {
        let stdout = io::stdout();
        let terminal = stdout.as_fd();
        if !rustix::termios::isatty(terminal) {
            return Ok(None);
        }
        let stdin = io::stdin();
        let stderr = io::stderr();
        let captures_stdin = converged && shares_terminal(terminal, stdin.as_fd());
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
        let real_modes = tcgetattr(terminal)?;
        if converged {
            tcsetattr(&slave, OptionalActions::Now, &real_modes)?;
        } else {
            let mut slave_modes = real_modes.clone();
            slave_modes
                .output_modes
                .remove(rustix::termios::OutputModes::OPOST);
            tcsetattr(&slave, OptionalActions::Now, &slave_modes)?;
        }
        let size = tcgetwinsize(terminal).ok();
        if let Some(size) = size {
            tcsetwinsize(&slave, size)?;
        }
        fcntl_setfl(&master, OFlags::NONBLOCK)?;

        let relay_terminal = rustix::io::fcntl_dupfd_cloexec(terminal, 3)?;
        let slave_poll = rustix::io::fcntl_dupfd_cloexec(&slave, 3)?;
        let slave_ctl = if converged {
            Some(rustix::io::fcntl_dupfd_cloexec(&slave, 3)?)
        } else {
            None
        };
        let saved_stdin = if captures_stdin {
            Some(rustix::io::fcntl_dupfd_cloexec(stdin.as_fd(), 3)?)
        } else {
            None
        };
        let saved_stdout = rustix::io::fcntl_dupfd_cloexec(terminal, 3)?;
        let saved_stderr = if captures_stderr {
            Some(rustix::io::fcntl_dupfd_cloexec(stderr.as_fd(), 3)?)
        } else {
            None
        };
        let real_raw = if converged {
            let mut raw = real_modes.clone();
            raw.make_raw();
            tcsetattr(terminal, OptionalActions::Now, &raw)?;
            Some(raw)
        } else {
            None
        };
        let restore_real = if converged { Some(real_modes) } else { None };
        #[cfg(target_os = "macos")]
        let (stop_read, stop_write) = crate::sys::pipe_cloexec_owned()?;
        #[cfg(not(target_os = "macos"))]
        let (stop_read, stop_write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        let paused = Arc::new(AtomicBool::new(false));
        let relay_paused = Arc::clone(&paused);
        let thread = std::thread::Builder::new()
            .name("ish-output-relay".into())
            .spawn(move || {
                relay(RelayArgs {
                    master,
                    stop: stop_read,
                    terminal: relay_terminal,
                    slave: slave_poll,
                    applied_size: size,
                    buffer,
                    forward_input: converged,
                    paused: relay_paused,
                    restore_real,
                })
            })?;
        let session = Session {
            stop: stop_write,
            thread: Some(thread),
            converged,
            paused,
            slave_ctl,
            real_raw,
        };

        use std::io::Write;
        let _ = io::stdout().flush();
        let restore = Restore { stdin: saved_stdin, stdout: saved_stdout, stderr: saved_stderr };
        if captures_stdin {
            rustix::stdio::dup2_stdin(&slave)?;
        }
        rustix::stdio::dup2_stdout(&slave)?;
        if captures_stderr
            && let Err(error) = rustix::stdio::dup2_stderr(&slave)
        {
            restore.apply();
            return Err(error.into());
        }
        Ok(Some(Self { restore: Some(restore), session: Some(session) }))
    }

    /// Give the shell its own standard streams back.
    pub fn end(mut self) -> Session {
        if let Some(restore) = self.restore.take() {
            restore.apply();
        }
        self.session.take().expect("a capture holds its session until it ends")
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn openpt_cloexec() -> io::Result<OwnedFd> {
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
pub(crate) fn openpt_cloexec() -> io::Result<OwnedFd> {
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

struct RelayArgs {
    master: OwnedFd,
    stop: OwnedFd,
    terminal: OwnedFd,
    slave: OwnedFd,
    applied_size: Option<Winsize>,
    buffer: OutputBuffer,
    forward_input: bool,
    paused: Arc<AtomicBool>,
    restore_real: Option<Termios>,
}

fn relay(args: RelayArgs) -> Finished {
    let RelayArgs {
        master,
        stop,
        terminal,
        slave,
        mut applied_size,
        buffer,
        forward_input,
        paused,
        restore_real,
    } = args;
    let mut pump = Pump {
        master,
        terminal,
        buffer,
        error: None,
        open: true,
        input_tail: Vec::new(),
        input_blocked: false,
    };
    let mut last_slave = tcgetattr(&slave).ok();
    let mut chunk = [0u8; RELAY_CHUNK];
    // Once the real terminal hangs up (the pane went away while the child
    // still runs), poll reports HUP/ERR on it forever. Keep polling it and
    // the relay burns a core without ever blocking, so drop it from the
    // poll set instead and wait on the master until the child exits.
    let mut terminal_alive = true;
    loop {
        let forwarding = forward_input && pump.open && !paused.load(Ordering::Acquire);
        let flush_tail = forwarding && !pump.input_tail.is_empty() && !pump.input_blocked;
        let watch_terminal = forwarding && pump.input_tail.is_empty() && terminal_alive;
        // Index 0 is always the stop pipe; index 1 is the master while it is
        // open; the real terminal is watched last only when a new read is allowed.
        let mut master_flags = PollFlags::IN;
        if flush_tail {
            master_flags |= PollFlags::OUT;
        }
        let mut fds = [
            PollFd::new(&stop, PollFlags::IN),
            PollFd::new(&pump.master, master_flags),
            PollFd::new(&pump.terminal, PollFlags::IN),
        ];
        let watched = 1 + usize::from(pump.open) + usize::from(watch_terminal && pump.open);
        match poll(&mut fds[..watched], Some(&RESIZE_POLL)) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => {
                pump.error.get_or_insert(error.into());
                break;
            }
        }
        let stopping = !fds[0].revents().is_empty();
        let master_events = if pump.open { fds[1].revents() } else { PollFlags::empty() };
        let master_readable = master_events.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
        let term_events = if watch_terminal && pump.open {
            fds[watched - 1].revents()
        } else {
            PollFlags::empty()
        };
        if master_readable {
            pump.drain(&mut chunk, usize::MAX, terminal_alive);
        }
        if stopping {
            if pump.open {
                pump.drain(&mut chunk, OUTPUT_LIMIT, terminal_alive);
            }
            break;
        }
        if pump.open
            && flush_tail
            && !paused.load(Ordering::Acquire)
            && master_events.intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP)
        {
            pump.drain_tail(&paused, &slave);
        }
        if !term_events.is_empty() {
            if term_events.intersects(PollFlags::IN) {
                if !pump.forward(&mut chunk, &stop, &paused, &slave) {
                    terminal_alive = false;
                }
            } else {
                // HUP/ERR with no readable input left: the terminal hung
                // up. Its readiness never clears, so stop watching it
                // rather than waking on every poll.
                terminal_alive = false;
            }
        }
        if forward_input {
            // A resize must reach the converged child through its own
            // terminal, whose foreground group differs from the shell's.
            // Signal only on an actual size change: the child redraws on
            // every WINCH, and an unconditional signal would keep the master
            // readable forever as each redraw wakes the relay again.
            let before = applied_size;
            follow_resize(&pump.terminal, &pump.master, &mut applied_size);
            if applied_size != before
                && let Some(foreground) = foreground_group(&pump.master)
                && foreground != rustix::process::getpgrp()
            {
                let _ = rustix::process::kill_process_group(foreground, rustix::process::Signal::WINCH);
            }
        } else {
            follow_resize(&pump.terminal, &pump.master, &mut applied_size);
            mirror_modes(&slave, &pump.terminal, &mut last_slave);
        }
    }
    if let Some(startup) = restore_real {
        let _ = tcsetattr(&pump.terminal, OptionalActions::Now, &startup);
    }
    Finished { buffer: pump.buffer, relay_error: pump.error, input_tail: pump.input_tail }
}

struct Pump {
    master: OwnedFd,
    terminal: OwnedFd,
    buffer: OutputBuffer,
    error: Option<io::Error>,
    open: bool,
    input_tail: Vec<u8>,
    input_blocked: bool,
}

impl Pump {
    fn drain(&mut self, chunk: &mut [u8], mut budget: usize, terminal_alive: bool) {
        while budget > 0 {
            match rustix::io::read(&self.master, &mut *chunk) {
                Ok(0) | Err(Errno::IO) => {
                    self.open = false;
                    return;
                }
                Ok(read) => {
                    budget = budget.saturating_sub(read);
                    self.buffer.push(&chunk[..read]);
                    // A hung-up terminal takes no output; the bytes stay
                    // captured in the buffer. Writing anyway would fail on
                    // every chunk without ever clearing poll readiness.
                    if terminal_alive
                        && self.error.is_none()
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

    /// Copy typed input from the real terminal to the capture terminal, where
    /// the foreground child reads it.
    /// Suspend characters are consumed and delivered as SIGSTOP to the slave's
    /// foreground group instead: the kernel discards terminal-generated stops
    /// for orphaned process groups, and every converged child is orphaned.
    /// SIGSTOP cannot be caught or discarded, so the stop always lands.
    /// Returns whether the real terminal is still usable. A hangup or I/O
    /// error reports `false` so the caller stops polling a descriptor whose
    /// readiness never clears.
    fn forward(&mut self, chunk: &mut [u8], stop: &OwnedFd, paused: &Arc<AtomicBool>, slave: &OwnedFd) -> bool {
        let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
        loop {
            if paused.load(Ordering::Acquire) || self.input_blocked {
                return true;
            }
            let mut ready = [
                PollFd::new(stop, PollFlags::IN),
                PollFd::new(&self.master, PollFlags::OUT),
                PollFd::new(&self.terminal, PollFlags::IN),
            ];
            if poll(&mut ready, Some(&zero)).is_err() {
                return true;
            }
            if !ready[0].revents().is_empty() || paused.load(Ordering::Acquire) {
                return true;
            }
            if ready[1].revents().is_empty() || ready[2].revents().is_empty() {
                return true;
            }
            if !ready[2].revents().intersects(PollFlags::IN) {
                return false;
            }
            match rustix::io::read(&self.terminal, &mut *chunk) {
                Ok(0) => return false,
                Ok(read) => {
                    self.forward_chunk(&chunk[..read], paused, slave);
                    if !self.input_tail.is_empty() || self.input_blocked {
                        return true;
                    }
                }
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => return true,
                Err(_) => return false,
            }
            if !self.open {
                return true;
            }
        }
    }

    fn drain_tail(&mut self, paused: &Arc<AtomicBool>, slave: &OwnedFd) {
        if self.input_tail.is_empty() || self.input_blocked {
            return;
        }
        let pending = std::mem::take(&mut self.input_tail);
        self.forward_chunk(&pending, paused, slave);
    }

    /// Forward one input chunk, turning line-discipline signal characters
    /// into signals the kernel would otherwise discard. The suspend
    /// character becomes SIGSTOP for the slave's foreground group: the
    /// kernel discards terminal-generated stops for orphaned process
    /// groups, and every converged child is orphaned. Interrupt and quit
    /// characters reach a converged child through the slave line
    /// discipline, but an in-process builtin owns no foreground group, so
    /// the relay signals the shell itself for those instead — mirroring
    /// what split capture's real terminal would do. Bytes the master does
    /// not accept stay in [`Pump::input_tail`].
    fn forward_chunk(&mut self, chunk: &[u8], paused: &Arc<AtomicBool>, slave: &OwnedFd) {
        let suspend = suspend_key(slave, &self.master);
        let interrupt = interrupt_keys(slave, &self.master);
        let mut rest = chunk;
        while !rest.is_empty() {
            if paused.load(Ordering::Acquire) {
                self.input_tail.extend_from_slice(rest);
                return;
            }
            let Some((found, action)) = earliest_signal(rest, suspend, interrupt) else {
                self.keep_unwritten(rest, paused);
                return;
            };
            if found > 0 {
                let outcome = write_master(&self.master, &rest[..found], paused);
                if outcome.blocked {
                    self.input_blocked = true;
                }
                if outcome.accepted < found || outcome.blocked {
                    self.input_tail.extend_from_slice(&rest[outcome.accepted..]);
                    return;
                }
            }
            match action {
                SignalAction::Stop(foreground) => {
                    let _ = rustix::process::kill_process_group(foreground, rustix::process::Signal::STOP);
                }
                SignalAction::Interrupt(signal) => {
                    let _ = rustix::process::kill_process_group(rustix::process::getpgrp(), signal);
                }
            }
            rest = &rest[found + 1..];
        }
    }

    fn keep_unwritten(&mut self, bytes: &[u8], paused: &Arc<AtomicBool>) {
        if bytes.is_empty() {
            return;
        }
        let outcome = write_master(&self.master, bytes, paused);
        if outcome.blocked {
            self.input_blocked = true;
        }
        if outcome.accepted < bytes.len() {
            self.input_tail.extend_from_slice(&bytes[outcome.accepted..]);
        }
    }
}

/// What to do when a signal character is found in forwarded input.
#[derive(Clone, Copy)]
enum SignalAction {
    Stop(rustix::process::Pid),
    Interrupt(rustix::process::Signal),
}

/// The earliest signal character in `rest` and what it should do.
fn earliest_signal(
    rest: &[u8],
    suspend: Option<(u8, rustix::process::Pid)>,
    interrupt: Option<(u8, u8)>,
) -> Option<(usize, SignalAction)> {
    let mut best: Option<(usize, SignalAction)> = None;
    if let Some((key, foreground)) = suspend
        && let Some(at) = rest.iter().position(|&byte| byte == key)
    {
        best = Some((at, SignalAction::Stop(foreground)));
    }
    if let Some((intr, quit)) = interrupt {
        for (key, signal) in [
            (intr, rustix::process::Signal::INT),
            (quit, rustix::process::Signal::QUIT),
        ] {
            if key == 0xff {
                continue;
            }
            if let Some(at) = rest.iter().position(|&byte| byte == key)
                && best.is_none_or(|(found, _)| at < found)
            {
                best = Some((at, SignalAction::Interrupt(signal)));
            }
        }
    }
    best
}

/// The slave's suspend key and foreground group, when the line discipline
/// would turn that key into SIGTSTP itself. Converged children live in a
/// session of their own, so the kernel discards terminal-generated stops
/// for them as orphaned; the relay delivers SIGSTOP directly instead, which
/// the kernel never discards for orphaned groups.
/// The foreground group is read through the master: macOS rejects the query
/// on the slave side.
fn suspend_key(slave: &OwnedFd, master: &OwnedFd) -> Option<(u8, rustix::process::Pid)> {
    let modes = tcgetattr(slave).ok()?;
    if !modes.local_modes.contains(rustix::termios::LocalModes::ISIG) {
        return None;
    }
    let key = modes.special_codes[rustix::termios::SpecialCodeIndex::VSUSP];
    if key == 0xff {
        return None;
    }
    let foreground = foreground_group(master)?;
    if foreground == rustix::process::getpgrp() {
        return None;
    }
    Some((key, foreground))
}

/// Interrupt and quit keys that should signal the shell itself. When a
/// converged child owns the slave, its line discipline turns these keys
/// into signals, so the relay just forwards the bytes. An in-process
/// builtin owns no foreground group, so the relay delivers SIGINT/SIGQUIT
/// to the shell's group instead — mirroring what split capture's real
/// terminal would do. Suspend has no fallback: stopping mid-copy is
/// refused by design, and the byte stays queued.
fn interrupt_keys(slave: &OwnedFd, master: &OwnedFd) -> Option<(u8, u8)> {
    let modes = tcgetattr(slave).ok()?;
    if !modes.local_modes.contains(rustix::termios::LocalModes::ISIG) {
        return None;
    }
    if foreground_group(master).is_some_and(|foreground| foreground != rustix::process::getpgrp()) {
        return None;
    }
    Some((
        modes.special_codes[rustix::termios::SpecialCodeIndex::VINTR],
        modes.special_codes[rustix::termios::SpecialCodeIndex::VQUIT],
    ))
}

struct MasterWrite {
    accepted: usize,
    blocked: bool,
}

/// Write input bytes to the capture master. `accepted` is how many bytes the
/// master took. `blocked` means the rest cannot be retried; the caller still
/// keeps those bytes. A short write or a pause leaves `blocked` clear so the
/// next relay iteration retries the suffix.
fn write_master(master: &OwnedFd, bytes: &[u8], paused: &Arc<AtomicBool>) -> MasterWrite {
    let mut accepted = 0;
    while accepted < bytes.len() {
        if paused.load(Ordering::Acquire) {
            return MasterWrite { accepted, blocked: false };
        }
        match rustix::io::write(master, &bytes[accepted..]) {
            Ok(0) => return MasterWrite { accepted, blocked: true },
            Ok(written) => accepted += written,
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => return MasterWrite { accepted, blocked: false },
            Err(_) => return MasterWrite { accepted, blocked: true },
        }
    }
    MasterWrite { accepted, blocked: false }
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
    if let Some(foreground) = foreground_group(terminal)
        && foreground != rustix::process::getpgrp()
    {
        let _ = rustix::process::kill_process_group(foreground, rustix::process::Signal::WINCH);
    }
}

/// The terminal's foreground process group, if it has one. A fresh capture
/// terminal has none until the child claims it, and the kernel reports that
/// as pgid 0 rather than an error; rustix turns that into a failed assertion,
/// so read it with libc directly.
fn foreground_group(terminal: &OwnedFd) -> Option<rustix::process::Pid> {
    use std::os::fd::AsRawFd;
    let pgid = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
    if pgid <= 0 {
        return None;
    }
    rustix::process::Pid::from_raw(pgid)
}

fn mirror_modes(slave: &OwnedFd, terminal: &OwnedFd, last_slave: &mut Option<rustix::termios::Termios>) {
    let Some(foreground) = foreground_group(terminal) else {
        return;
    };
    if foreground == rustix::process::getpgrp() {
        return;
    }
    let Ok(current) = tcgetattr(slave) else {
        return;
    };
    if let Some(previous) = last_slave
        && termios_same(&current, previous)
    {
        return;
    }
    let mut desired = current.clone();
    desired
        .output_modes
        .remove(rustix::termios::OutputModes::OPOST);
    let _ = tcsetattr(terminal, OptionalActions::Now, &desired);
    *last_slave = Some(current);
}

fn termios_same(first: &rustix::termios::Termios, second: &rustix::termios::Termios) -> bool {
    first.input_modes == second.input_modes
        && first.output_modes == second.output_modes
        && first.control_modes == second.control_modes
        && first.local_modes == second.local_modes
        && first.input_speed() == second.input_speed()
        && first.output_speed() == second.output_speed()
        && special_same(first, second)
}

fn special_same(first: &rustix::termios::Termios, second: &rustix::termios::Termios) -> bool {
    use rustix::termios::SpecialCodeIndex as Index;
    first.special_codes[Index::VMIN] == second.special_codes[Index::VMIN]
        && first.special_codes[Index::VTIME] == second.special_codes[Index::VTIME]
        && first.special_codes[Index::VINTR] == second.special_codes[Index::VINTR]
        && first.special_codes[Index::VQUIT] == second.special_codes[Index::VQUIT]
        && first.special_codes[Index::VERASE] == second.special_codes[Index::VERASE]
        && first.special_codes[Index::VKILL] == second.special_codes[Index::VKILL]
        && first.special_codes[Index::VEOF] == second.special_codes[Index::VEOF]
        && first.special_codes[Index::VSTART] == second.special_codes[Index::VSTART]
        && first.special_codes[Index::VSTOP] == second.special_codes[Index::VSTOP]
        && first.special_codes[Index::VSUSP] == second.special_codes[Index::VSUSP]
        && first.special_codes[Index::VEOL] == second.special_codes[Index::VEOL]
        && first.special_codes[Index::VEOL2] == second.special_codes[Index::VEOL2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fd::OwnedFd;
    use rustix::io::Errno;
    use std::sync::{Arc, atomic::AtomicBool};

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

    fn test_pump(terminal: OwnedFd, master: OwnedFd) -> (Pump, OwnedFd, OwnedFd) {
        let (stop_read, stop_write) = rustix::pipe::pipe().expect("stop pipe");
        let pump = Pump {
            master,
            terminal,
            buffer: OutputBuffer::default(),
            error: None,
            open: true,
            input_tail: Vec::new(),
            input_blocked: false,
        };
        (pump, stop_read, stop_write)
    }

    #[test]
    fn forward_reports_a_hung_up_terminal_as_dead() {
        let (terminal_read, terminal_write) = rustix::pipe::pipe().expect("terminal pipe");
        drop(terminal_write);
        let (master_read, master_write) = rustix::pipe::pipe().expect("master pipe");
        let (mut pump, stop, _stop_write) = test_pump(terminal_read, master_write);
        let paused = Arc::new(AtomicBool::new(false));
        let mut chunk = [0u8; RELAY_CHUNK];
        assert!(
            !pump.forward(&mut chunk, &stop, &paused, &master_read),
            "a terminal at EOF must read as dead so the relay stops polling it"
        );
        assert!(pump.input_tail.is_empty());
    }

    #[test]
    fn forward_keeps_a_live_terminal_and_delivers_its_input() {
        let (terminal_read, terminal_write) = rustix::pipe::pipe().expect("terminal pipe");
        let (master_read, master_write) = rustix::pipe::pipe().expect("master pipe");
        rustix::fs::fcntl_setfl(&master_read, rustix::fs::OFlags::NONBLOCK).expect("nonblock master");
        let (mut pump, stop, _stop_write) = test_pump(terminal_read, master_write);
        let paused = Arc::new(AtomicBool::new(false));
        rustix::io::write(&terminal_write, b"hi").expect("type input");
        let mut chunk = [0u8; RELAY_CHUNK];
        assert!(
            pump.forward(&mut chunk, &stop, &paused, &master_read),
            "a readable terminal must stay watched"
        );
        assert!(pump.input_tail.is_empty());
        assert_eq!(read_available(&master_read), b"hi");
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

    #[test]
    fn short_master_write_keeps_the_suffix_including_interrupt() {
        let (master_read, master_write) = rustix::pipe::pipe().expect("pipe");
        rustix::fs::fcntl_setfl(&master_read, rustix::fs::OFlags::NONBLOCK).expect("nonblock read");
        rustix::fs::fcntl_setfl(&master_write, rustix::fs::OFlags::NONBLOCK).expect("nonblock write");
        let (slave_read, slave_write) = rustix::pipe::pipe().expect("slave pipe");
        let capacity = write_until_full(&master_write);
        let space = 64;
        assert!(capacity > space);
        let _ = read_exact(&master_read, space);
        let mut payload = vec![b'a'; 800];
        payload.push(0x03);
        let paused = Arc::new(AtomicBool::new(false));
        let mut pump = Pump {
            master: master_write,
            terminal: slave_write,
            buffer: OutputBuffer::default(),
            error: None,
            open: true,
            input_tail: Vec::new(),
            input_blocked: false,
        };
        pump.forward_chunk(&payload, &paused, &slave_read);
        assert!(!pump.input_blocked);
        assert!(!pump.input_tail.is_empty(), "short write dropped its suffix");
        assert_eq!(pump.input_tail.last().copied(), Some(0x03));
        let prefix_len = payload.len() - pump.input_tail.len();
        assert!(prefix_len > 0, "write accepted no prefix");
        assert_eq!(pump.input_tail.as_slice(), &payload[prefix_len..]);
        let suffix = pump.input_tail.clone();
        let _filler = read_exact(&master_read, capacity - space);
        let written_prefix = read_exact(&master_read, prefix_len);
        assert_eq!(written_prefix, payload[..prefix_len]);
        pump.drain_tail(&paused, &slave_read);
        assert!(pump.input_tail.is_empty(), "suffix was not written on retry");
        assert!(!pump.input_blocked);
        let written_suffix = read_available(&master_read);
        assert_eq!(written_suffix, suffix);
        assert_eq!(written_suffix.last().copied(), Some(0x03));
    }

    fn write_until_full(fd: &OwnedFd) -> usize {
        let filler = vec![0u8; 64 * 1024];
        let mut total = 0;
        loop {
            match rustix::io::write(fd, &filler) {
                Ok(0) => return total,
                Ok(written) => total += written,
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => return total,
                Err(error) => panic!("fill pipe: {error}"),
            }
        }
    }

    fn read_exact(fd: &OwnedFd, mut needed: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(needed);
        let mut buf = [0u8; 4096];
        while needed > 0 {
            let take = needed.min(buf.len());
            match rustix::io::read(fd, &mut buf[..take]) {
                Ok(0) => panic!("eof after {} of {needed} still needed", out.len()),
                Ok(read) => {
                    out.extend_from_slice(&buf[..read]);
                    needed -= read;
                }
                Err(Errno::INTR) => {}
                Err(error) => panic!("read: {error}"),
            }
        }
        out
    }

    fn read_available(fd: &OwnedFd) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match rustix::io::read(fd, &mut buf) {
                Ok(0) => break,
                Ok(read) => out.extend_from_slice(&buf[..read]),
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => break,
                Err(error) => panic!("read: {error}"),
            }
        }
        out
    }
}
