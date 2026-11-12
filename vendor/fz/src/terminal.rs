// Terminal behavior derived from fzy, copyright (c) 2014 John Hawthorn.
// SPDX-License-Identifier: MIT. See LICENSE.

//! The small POSIX boundary used by the interactive selector.
//!
//! The editor deliberately stores the query as bytes because fzy matches bytes.
//! Cursor movement and deletion only step at UTF-8 code point boundaries, so a
//! terminal user never lands in the middle of a normally encoded character.

#[cfg(target_os = "linux")]
use rustix::{
    event::{self, PollFd, PollFlags, Timespec},
    io::{self as rustix_io, Errno},
};
#[cfg(target_os = "macos")]
use rustix::io::Errno;
use rustix::termios::{self, InputModes, LocalModes, OptionalActions, Termios};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::marker::PhantomData;
use std::mem;
#[cfg(target_os = "linux")]
use std::mem::MaybeUninit;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "macos")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "macos")]
use std::os::unix::thread::JoinHandleExt;
#[cfg(target_os = "macos")]
use std::rc::Rc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::thread::JoinHandle;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::rc::Rc;
use std::time::{Duration, Instant};

#[cfg(not(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("fz's terminal boundary supports Linux and macOS on x86_64 or aarch64");

#[cfg(target_os = "macos")]
struct ResizeEvents {
    wake_read: OwnedFd,
    wake_write: OwnedFd,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    previous: libc::sigaction,
    original_mask: libc::sigset_t,
    installed: bool,
    // pthread_sigmask is thread-local. Restoring it on another thread would
    // leak the blocked SIGWINCH into whichever thread dropped the terminal.
    _thread_bound: PhantomData<Rc<()>>,
}

#[cfg(target_os = "macos")]
extern "C" fn discard_winch(_: libc::c_int) {}

#[cfg(target_os = "linux")]
struct ResizeEvents {
    signal_fd: OwnedFd,
    // `pthread_sigmask` is thread-local. The thread that installs this
    // descriptor must also restore this exact complete mask.
    original_mask: Option<libc::sigset_t>,
    // `sigset_t` itself is Send, but moving it would restore one thread's
    // mask on another thread. Keep the whole Terminal on its opening thread.
    _thread_bound: PhantomData<Rc<()>>,
}

#[cfg(target_os = "linux")]
impl ResizeEvents {
    fn install(_: &File) -> io::Result<Self> {
        let winch_mask = winch_mask()?;
        // Some libc ABIs write only the kernel-sized prefix of sigset_t. Its
        // all-zero representation is a valid empty mask, so pre-zero unused
        // ABI storage before Rust later treats the full value as initialized.
        let mut original_mask = MaybeUninit::<libc::sigset_t>::zeroed();
        // SAFETY: `winch_mask` is initialized and `original_mask` is writable.
        // pthread_sigmask returns its error number directly rather than errno.
        let mask_result = unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &winch_mask, original_mask.as_mut_ptr())
        };
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }
        // SAFETY: pthread_sigmask initialized the kernel-sized prefix; the
        // zeroed remainder was already initialized as valid mask storage.
        let original_mask = unsafe { original_mask.assume_init() };

        // SIGWINCH remains blocked for this terminal's lifetime. A resize
        // arriving while this descriptor is created or while poll starts is
        // pending, so it makes this descriptor readable instead of racing a
        // handler's check-before-poll window.
        // SAFETY: -1 requests a new descriptor, `winch_mask` is initialized,
        // and these are valid Linux signalfd flags.
        let raw_fd = unsafe {
            libc::signalfd(
                -1,
                &winch_mask,
                libc::SFD_CLOEXEC | libc::SFD_NONBLOCK,
            )
        };
        if raw_fd == -1 {
            let signal_error = io::Error::last_os_error();
            return match restore_signal_mask(&original_mask) {
                Ok(()) => Err(signal_error),
                Err(restore_error) => Err(restore_error),
            };
        }

        // SAFETY: signalfd returned a newly owned nonnegative descriptor.
        let signal_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        Ok(Self {
            signal_fd,
            original_mask: Some(original_mask),
            _thread_bound: PhantomData,
        })
    }

    fn wait_for_input(&self, input: &File, termination: &crate::termination::Termination, timeout: Option<Duration>) -> io::Result<Wait> {
        let timeout = timeout.map(|timeout| {
            Timespec::try_from(timeout).expect("the fixed escape timeout fits in a timespec")
        });
        use std::os::fd::BorrowedFd;
        // SAFETY: the termination guard owns this descriptor for the entire wait.
        let wake = unsafe { BorrowedFd::borrow_raw(termination.fd()) };
        let mut descriptors = [
            PollFd::new(&wake, PollFlags::IN),
            PollFd::new(input, PollFlags::IN),
            PollFd::new(&self.signal_fd, PollFlags::IN),
        ];
        match event::poll(&mut descriptors, timeout.as_ref()) {
            Ok(0) => Ok(Wait::TimedOut),
            Ok(_) if descriptors[0].revents().contains(PollFlags::IN) => Ok(Wait::Interrupted),
            Ok(_) if descriptors[2].revents().contains(PollFlags::IN) => {
                self.drain_signals()?;
                // Input may be ready too. Redraw first; it stays readable for
                // the next iteration, which preserves the resize transition.
                Ok(Wait::Interrupted)
            }
            Ok(_) if descriptors[2].revents().is_empty() => Ok(Wait::Ready),
            Ok(_) => Err(io::Error::other("signalfd stopped reporting SIGWINCH")),
            // Other signals can still interrupt poll. Redraw and retry.
            Err(Errno::INTR) => Ok(Wait::Interrupted),
            Err(error) => Err(error.into()),
        }
    }

    fn drain_signals(&self) -> io::Result<()> {
        // signalfd requires a buffer at least as large as this ABI-owned
        // record. Its payload is intentionally unused: this descriptor only
        // accepts SIGWINCH, and every queued record means redraw.
        let mut record = [0_u8; mem::size_of::<libc::signalfd_siginfo>()];
        loop {
            match rustix_io::read(&self.signal_fd, &mut record) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "signalfd reached EOF",
                    ));
                }
                Ok(bytes) if bytes == record.len() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "signalfd returned a partial signal record",
                    ));
                }
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let Some(original_mask) = self.original_mask.take() else {
            return Ok(());
        };
        if let Err(error) = restore_signal_mask(&original_mask) {
            // Retain it so Terminal::drop can retry after an explicit restore
            // failure. The descriptor stays open until this field drops.
            self.original_mask = Some(original_mask);
            return Err(error);
        }
        Ok(())
    }

    fn input_pending(&self, input: &File) -> io::Result<bool> {
        let timeout = Timespec::try_from(Duration::ZERO).expect("a zero timeout fits in a timespec");
        let mut descriptors = [PollFd::new(input, PollFlags::IN)];
        match event::poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(descriptors[0].revents().contains(PollFlags::IN)),
            Err(Errno::INTR) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ResizeEvents {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(target_os = "linux")]
fn winch_mask() -> io::Result<libc::sigset_t> {
    // See install: a zero mask is valid and initializes ABI storage libc may
    // not touch outside the kernel-sized signal-set prefix.
    let mut mask = MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: the libc functions initialize the supplied writable sigset_t,
    // and SIGWINCH is a valid Linux signal number.
    if unsafe { libc::sigemptyset(mask.as_mut_ptr()) } == -1
        || unsafe { libc::sigaddset(mask.as_mut_ptr(), libc::SIGWINCH) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zeroed storage was already initialized, and the libc calls set
    // the mask's required kernel-visible bits.
    Ok(unsafe { mask.assume_init() })
}

#[cfg(target_os = "linux")]
fn restore_signal_mask(mask: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: `mask` was captured by pthread_sigmask for this same thread and
    // restores its complete caller-owned signal mask.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

#[cfg(target_os = "macos")]
fn configure_fd(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is an open descriptor owned by the caller.
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if fd_flags == -1
        || status == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn winch_mask() -> io::Result<libc::sigset_t> {
    let mut mask: libc::sigset_t = unsafe { mem::zeroed() };
    if unsafe { libc::sigemptyset(&mut mask) } == -1
        || unsafe { libc::sigaddset(&mut mask, libc::SIGWINCH) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(mask)
}

#[cfg(target_os = "macos")]
fn serve_winch(write_fd: libc::c_int, stop: Arc<AtomicBool>) {
    let Ok(mask) = winch_mask() else {
        unsafe { libc::close(write_fd) };
        return;
    };
    while !stop.load(Ordering::Acquire) {
        let mut signal = 0;
        // SAFETY: `mask` contains SIGWINCH, which is blocked in this thread.
        // sigwait dequeues it without running a handler.
        let result = unsafe { libc::sigwait(&mask, &mut signal) };
        if result != 0 || stop.load(Ordering::Acquire) {
            break;
        }
        let token = [1u8];
        unsafe {
            libc::write(write_fd, token.as_ptr().cast(), 1);
        }
    }
    unsafe { libc::close(write_fd) };
}

#[cfg(target_os = "macos")]
impl ResizeEvents {
    fn install(_: &File) -> io::Result<Self> {
        // A SIGWINCH handler that writes while the main thread is inside a tty
        // operation can wedge the process so the bare-Escape wait never
        // returns. SIGWINCH stays blocked. Its default disposition is ignore,
        // which would discard it before sigwait, so a no-op handler marks it
        // catchable and the blocked mask keeps that handler from running.
        let mut pipes = [0; 2];
        // SAFETY: `pipes` is writable storage for two descriptors. On failure
        // neither entry is owned.
        if unsafe { libc::pipe(pipes.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if let Err(error) = configure_fd(pipes[0]).and_then(|()| configure_fd(pipes[1])) {
            unsafe {
                libc::close(pipes[0]);
                libc::close(pipes[1]);
            }
            return Err(error);
        }
        // SAFETY: `pipes[1]` is the open write end. The duplicate is owned by
        // the sigwait thread, which closes it when that thread exits.
        let thread_write = unsafe { libc::dup(pipes[1]) };
        if thread_write == -1 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(pipes[0]);
                libc::close(pipes[1]);
            }
            return Err(error);
        }
        if let Err(error) = configure_fd(thread_write) {
            unsafe {
                libc::close(pipes[0]);
                libc::close(pipes[1]);
                libc::close(thread_write);
            }
            return Err(error);
        }
        // SAFETY: pipe returned two owned descriptors, and both are still open.
        let wake_read = unsafe { OwnedFd::from_raw_fd(pipes[0]) };
        let wake_write = unsafe { OwnedFd::from_raw_fd(pipes[1]) };

        let mask = match winch_mask() {
            Ok(mask) => mask,
            Err(error) => {
                unsafe { libc::close(thread_write) };
                return Err(error);
            }
        };
        let mut original_mask: libc::sigset_t = unsafe { mem::zeroed() };
        // SAFETY: `mask` is initialized. pthread_sigmask writes the previous
        // mask and returns its error number instead of setting errno.
        let mask_result = unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut original_mask)
        };
        if mask_result != 0 {
            unsafe { libc::close(thread_write) };
            return Err(io::Error::from_raw_os_error(mask_result));
        }

        // SAFETY: both sigaction values are zeroed. `discard_winch` is an
        // `extern "C"` function and is not invoked while SIGWINCH stays blocked.
        let mut action: libc::sigaction = unsafe { mem::zeroed() };
        action.sa_sigaction = discard_winch as *const () as libc::sighandler_t;
        let mut previous: libc::sigaction = unsafe { mem::zeroed() };
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1
            || unsafe { libc::sigaction(libc::SIGWINCH, &action, &mut previous) } == -1
        {
            let error = io::Error::last_os_error();
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut());
                libc::close(thread_write);
            }
            return Err(error);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = match std::thread::Builder::new()
            .name("fz-sigwinch".to_string())
            .spawn(move || serve_winch(thread_write, thread_stop))
        {
            Ok(thread) => thread,
            Err(error) => {
                unsafe {
                    libc::sigaction(libc::SIGWINCH, &previous, std::ptr::null_mut());
                    libc::pthread_sigmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut());
                    libc::close(thread_write);
                }
                return Err(error);
            }
        };
        Ok(Self {
            wake_read,
            wake_write,
            stop,
            thread: Some(thread),
            previous,
            original_mask,
            installed: true,
            _thread_bound: PhantomData,
        })
    }

    fn wait_for_input(&self, input: &File, termination: &crate::termination::Termination, timeout: Option<Duration>) -> io::Result<Wait> {
        // macOS poll reports POLLNVAL for /dev/tty, the default terminal
        // device, so wait with select, which handles it.
        use rustix::event::{FdSetElement, Timespec, fd_set_insert, fd_set_num_elements, select, FdSetIter};
        use std::os::fd::{AsFd, AsRawFd};
        let wake = self.wake_read.as_fd().as_raw_fd();
        let tty = input.as_fd().as_raw_fd();
        let termination_fd = termination.fd();
        let nfds = wake.max(tty).max(termination_fd) + 1;
        let timeout = timeout.map(|timeout| {
            Timespec::try_from(timeout).expect("the fixed escape timeout fits in a timespec")
        });
        let mut readfds = vec![FdSetElement::default(); fd_set_num_elements(3, nfds)];
        fd_set_insert(&mut readfds, wake);
        fd_set_insert(&mut readfds, tty);
        fd_set_insert(&mut readfds, termination_fd);
        // SAFETY: both descriptors are owned by this terminal and stay open
        // for the call; the set fits every inserted descriptor.
        match unsafe { select(nfds, Some(&mut readfds), None, None, timeout.as_ref()) } {
            Ok(0) => Ok(Wait::TimedOut),
            Ok(_) => {
                let mut wake_ready = false;
                let mut tty_ready = false;
                for fd in FdSetIter::new(&readfds) {
                    if fd == termination_fd { return Ok(Wait::Interrupted); }
                    if fd == wake {
                        wake_ready = true;
                    } else if fd == tty {
                        tty_ready = true;
                    }
                }
                if wake_ready {
                    self.drain_wake()?;
                    // Input may be ready too. Redraw first; it stays readable
                    // for the next iteration, which preserves the resize transition.
                    Ok(Wait::Interrupted)
                } else if tty_ready {
                    Ok(Wait::Ready)
                } else {
                    Ok(Wait::TimedOut)
                }
            }
            Err(Errno::INTR) => Ok(Wait::Interrupted),
            Err(error) => Err(error.into()),
        }
    }

    fn drain_wake(&self) -> io::Result<()> {
        let mut buf = [0u8; 64];
        loop {
            match rustix::io::read(&self.wake_read, &mut buf) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "resize wake pipe closed",
                    ));
                }
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn input_pending(&self, input: &File) -> io::Result<bool> {
        use rustix::event::{FdSetElement, Timespec, fd_set_insert, fd_set_num_elements, select};
        use std::os::fd::{AsFd, AsRawFd};
        let tty = input.as_fd().as_raw_fd();
        let nfds = tty + 1;
        let timeout = Timespec::try_from(Duration::ZERO).expect("a zero timeout fits in a timespec");
        let mut readfds = vec![FdSetElement::default(); fd_set_num_elements(1, nfds)];
        fd_set_insert(&mut readfds, tty);
        // SAFETY: `tty` stays open for the call, and the set covers that descriptor.
        match unsafe { select(nfds, Some(&mut readfds), None, None, Some(&timeout)) } {
            Ok(0) => Ok(false),
            Ok(_) => Ok(true),
            Err(Errno::INTR) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.installed {
            return Ok(());
        }
        // The write end stays open until the sigwait thread has exited.
        let _ = &self.wake_write;
        self.stop.store(true, Ordering::Release);
        let mut join_failed = false;
        if let Some(thread) = self.thread.take() {
            // SAFETY: the thread is blocked in sigwait, or about to enter it,
            // with SIGWINCH blocked, so this directed signal unblocks sigwait.
            let kill_result = unsafe { libc::pthread_kill(thread.as_pthread_t(), libc::SIGWINCH) };
            join_failed = thread.join().is_err();
            if kill_result != 0 && kill_result != libc::ESRCH {
                let _ = unsafe {
                    libc::sigaction(libc::SIGWINCH, &self.previous, std::ptr::null_mut())
                };
                let _ = unsafe {
                    libc::pthread_sigmask(
                        libc::SIG_SETMASK,
                        &self.original_mask,
                        std::ptr::null_mut(),
                    )
                };
                self.installed = false;
                return Err(io::Error::from_raw_os_error(kill_result));
            }
        }
        // SAFETY: `previous` was captured from the successful install, and a
        // null old-action pointer only installs that action. The saved mask
        // belongs to this thread.
        if unsafe { libc::sigaction(libc::SIGWINCH, &self.previous, std::ptr::null_mut()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mask_result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.original_mask, std::ptr::null_mut())
        };
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }
        self.installed = false;
        if join_failed {
            return Err(io::Error::other("resize thread panicked"));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for ResizeEvents {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// A raw-mode terminal. Dropping it always puts its saved termios state back.
struct Foreground {
    terminal: File,
    group: rustix::process::Pid,
    ttou: libc::sigaction,
    active: bool,
}

impl Foreground {
    fn claim(terminal: &File) -> io::Result<Option<Self>> {
        let group = termios::tcgetpgrp(terminal)?;
        let current = rustix::process::getpgrp();
        if group == current { return Ok(None); }
        let terminal = terminal.try_clone()?;
        // SAFETY: sigaction records are valid, writable ABI storage; SIG_IGN
        // prevents job-control stops only during the explicit worker handoff.
        let mut action: libc::sigaction = unsafe { mem::zeroed() };
        action.sa_sigaction = libc::SIG_IGN;
        let mut ttou: libc::sigaction = unsafe { mem::zeroed() };
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1
            || unsafe { libc::sigaction(libc::SIGTTOU, &action, &mut ttou) } == -1
        { return Err(io::Error::last_os_error()); }
        let mut guard = Self { terminal, group, ttou, active: true };
        if let Err(error) = termios::tcsetpgrp(&guard.terminal, current) {
            let _ = guard.restore();
            return Err(error.into());
        }
        Ok(Some(guard))
    }
    fn restore(&mut self) -> io::Result<()> {
        if !self.active { return Ok(()); }
        termios::tcsetpgrp(&self.terminal, self.group)?;
        // SAFETY: ttou was captured in this process, and restores the caller's disposition.
        if unsafe { libc::sigaction(libc::SIGTTOU, &self.ttou, std::ptr::null_mut()) } == -1 { return Err(io::Error::last_os_error()); }
        self.active = false;
        Ok(())
    }
}
impl Drop for Foreground { fn drop(&mut self) { let _ = self.restore(); } }

pub struct Terminal {
    resize_events: ResizeEvents,
    termination: crate::termination::Termination,
    foreground: Option<Foreground>,
    input: File,
    pending: Option<File>,
    output: BufWriter<File>,
    original: Termios,
    restore_needed: bool,
    bracketed_paste: bool,
}

impl Terminal {
    /// Opens `path` for input and output, then switches input to fzy's raw mode.
    pub fn open(path: &Path, claim_foreground: bool, pending: Option<File>) -> io::Result<Self> {
        let input = OpenOptions::new().read(true).open(path)?;
        let output = OpenOptions::new().write(true).open(path)?;
        // select on a macOS tty can report readability when a following read
        // would block. A blocking read there never returns, so the descriptor
        // stays nonblocking and a false ready result goes back to the wait.
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let fd = input.as_raw_fd();
            // SAFETY: `fd` is the descriptor just opened for this terminal.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
        }

        let foreground = if claim_foreground { Foreground::claim(&input)? } else { None };
        let termination = crate::termination::Termination::install()?;
        let original = termios::tcgetattr(&input)?;
        let mut raw = original.clone();
        raw.input_modes.remove(InputModes::ICRNL);
        raw.local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG);
        termios::tcsetattr(&input, OptionalActions::Now, &raw)?;

        let resize_events = match ResizeEvents::install(&input) {
            Ok(resize_events) => resize_events,
            Err(error) => {
                let _ = termios::tcsetattr(&input, OptionalActions::Now, &original);
                return Err(error);
            }
        };

        let mut terminal = Self {
            resize_events,
            termination,
            foreground,
            pending,
            input,
            output: BufWriter::with_capacity(16 * 1024, output),
            original,
            restore_needed: true,
            bracketed_paste: claim_foreground,
        };
        if terminal.bracketed_paste { terminal.output.write_all(b"\x1b[?2004h")?; }
        terminal.output.write_all(b"\x1b[0m")?;
        terminal.output.flush()?;
        Ok(terminal)
    }

    pub fn run(&mut self, picker: &mut crate::Picker) -> io::Result<crate::Outcome> {
        if !self.restore_needed { return Err(io::Error::other("terminal has already been restored")); }
        let mut escape_deadline: Option<Instant> = None;
        let mut draw_needed = true;
        loop {
            if let Some(signal) = self.termination.pending() {
                self.restore()?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, crate::termination::Interrupted(signal)));
            }
            let (width, height) = self.dimensions();
            picker.resize(width, height)?;
            if draw_needed { picker.render(&mut self.output, width)?; }
            draw_needed = true;
            if picker.ambiguous_key_pending() {
                escape_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(25));
            } else { escape_deadline = None; }
            let timeout = escape_deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let ready = if timeout == Some(Duration::ZERO) { Wait::TimedOut } else { self.wait_for_input(timeout)? };
            let byte = match ready {
                Wait::Ready => match self.read_byte() {
                    Ok(Some(byte)) => Some(byte),
                    Ok(None) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error),
                },
                Wait::Interrupted => {
                    if self.resize_events.input_pending(&self.input)? { continue; }
                    if !escape_deadline.is_some_and(|deadline| Instant::now() >= deadline) { continue; }
                    None
                }
                Wait::TimedOut => None,
            };
            if let Some(outcome) = picker.feed(byte) {
                picker.clear(&mut self.output)?;
                self.restore()?;
                return Ok(outcome);
            }
            draw_needed = !self.resize_events.input_pending(&self.input)?;
        }
    }

    pub(crate) fn wait_candidate_input(&self, source: &File) -> io::Result<()> {
        use std::os::fd::BorrowedFd;
        use rustix::event::{PollFd, PollFlags};
        loop {
            if let Some(signal) = self.termination.pending() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, crate::termination::Interrupted(signal)));
            }
            // SAFETY: the termination guard owns this descriptor for the wait.
            let wake = unsafe { BorrowedFd::borrow_raw(self.termination.fd()) };
            let mut fds = [PollFd::new(source, PollFlags::IN), PollFd::new(&wake, PollFlags::IN)];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) if !fds[1].revents().is_empty() => continue,
                Ok(_) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn dimensions(&self) -> (usize, usize) {
        let Ok(size) = termios::tcgetwinsize(self.output.get_ref()) else {
            return (80, 25);
        };
        if size.ws_col == 0 || size.ws_row == 0 {
            return (80, 25);
        }
        (usize::from(size.ws_col), usize::from(size.ws_row))
    }

    fn wait_for_input(&self, timeout: Option<Duration>) -> io::Result<Wait> {
        if self.pending.is_some() { return Ok(Wait::Ready); }
        self.resize_events.wait_for_input(&self.input, &self.termination, timeout)
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        if let Some(pending) = self.pending.as_mut() {
            match pending.read(&mut byte)? {
                0 => { self.pending = None; return Err(io::ErrorKind::WouldBlock.into()); }
                _ => return Ok(Some(byte[0])),
            }
        }
        loop {
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let paste_result = if self.bracketed_paste {
            let result = self.output.write_all(b"\x1b[?2004l").and_then(|()| self.output.flush());
            if result.is_ok() { self.bracketed_paste = false; }
            result
        } else { Ok(()) };
        let termios_result = if self.restore_needed {
            loop {
                match termios::tcsetattr(&self.input, OptionalActions::Now, &self.original) {
                    Ok(()) => {
                        self.restore_needed = false;
                        break Ok(());
                    }
                    Err(error) => {
                        let error = io::Error::from(error);
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break Err(error);
                    }
                }
            }
        } else {
            Ok(())
        };
        let resize_result = self.resize_events.restore();
        let foreground_result = self.foreground.as_mut().map_or(Ok(()), Foreground::restore);
        let termination_result = self.termination.restore();
        paste_result.and(termios_result).and(resize_result).and(foreground_result).and(termination_result)
    }

}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

enum Wait { Ready, TimedOut, Interrupted }

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::ResizeEvents;
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;

    struct RestoreMask(libc::sigset_t);

    impl Drop for RestoreMask {
        fn drop(&mut self) {
            // The saved mask belongs to this test thread. Test cleanup must
            // not leak its temporary signal state into the harness.
            let _ = unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut())
            };
        }
    }

    fn current_mask() -> io::Result<libc::sigset_t> {
        // libc may leave unused sigset_t storage untouched; a zero mask is a
        // valid initial representation for that storage.
        let mut mask = MaybeUninit::<libc::sigset_t>::zeroed();
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), mask.as_mut_ptr())
        };
        if result == 0 {
            // SAFETY: pthread_sigmask filled its kernel-visible prefix and
            // the zeroed remainder was already initialized.
            Ok(unsafe { mask.assume_init() })
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn signal_set(signal: libc::c_int) -> io::Result<libc::sigset_t> {
        let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
        if unsafe { libc::sigemptyset(set.as_mut_ptr()) } == -1
            || unsafe { libc::sigaddset(set.as_mut_ptr(), signal) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed storage is a valid empty mask, and libc updated its
        // required signal-set prefix.
        Ok(unsafe { set.assume_init() })
    }

    fn change_signal_mask(how: libc::c_int, signal: libc::c_int) -> io::Result<()> {
        let set = signal_set(signal)?;
        let result = unsafe { libc::pthread_sigmask(how, &set, std::ptr::null_mut()) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn is_blocked(mask: &libc::sigset_t, signal: libc::c_int) -> bool {
        match unsafe { libc::sigismember(mask, signal) } {
            0 => false,
            1 => true,
            _ => panic!("sigismember({signal}) failed: {}", io::Error::last_os_error()),
        }
    }

    #[test]
    fn resize_events_restore_blocked_and_unblocked_caller_masks() -> io::Result<()> {
        let original = current_mask()?;
        let _restore_original = RestoreMask(original);

        // Preserve both legal caller states. A parent may already block
        // SIGWINCH, while an unrelated blocked signal proves restoration uses
        // the complete saved mask rather than only unblocking SIGWINCH.
        change_signal_mask(libc::SIG_BLOCK, libc::SIGUSR1)?;
        for caller_blocks_winch in [false, true] {
            change_signal_mask(
                if caller_blocks_winch {
                    libc::SIG_BLOCK
                } else {
                    libc::SIG_UNBLOCK
                },
                libc::SIGWINCH,
            )?;
            let expected_after_restore = current_mask()?;

            let input = File::open("/dev/null")?;
            let mut events = ResizeEvents::install(&input)?;
            let installed_mask = current_mask()?;
            assert!(is_blocked(&installed_mask, libc::SIGWINCH));
            assert!(is_blocked(&installed_mask, libc::SIGUSR1));

            events.restore()?;
            let restored_mask = current_mask()?;
            assert_eq!(
                is_blocked(&restored_mask, libc::SIGWINCH),
                caller_blocks_winch
            );
            assert_eq!(
                is_blocked(&restored_mask, libc::SIGUSR1),
                is_blocked(&expected_after_restore, libc::SIGUSR1)
            );
        }
        Ok(())
    }
}
