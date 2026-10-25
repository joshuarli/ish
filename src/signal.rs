use crate::sys;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicI32;

static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
static PIPE_READ: AtomicI32 = AtomicI32::new(-1);
static SHELL_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static PENDING_FOREGROUND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static PENDING_TERMINATION: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
static FOREGROUND_SIGNALS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn init() -> RawFd {
    // Capture the login terminal modes before any handler can need them; a
    // crash during startup must still restore what was there.
    crate::term::capture_startup_termios();
    let (read_fd, write_fd) =
        sys::pipe_nonblock_cloexec().expect("pipe() failed for signal self-pipe");

    PIPE_READ.store(read_fd, std::sync::atomic::Ordering::Release);
    PIPE_WRITE.store(write_fd, std::sync::atomic::Ordering::Release);

    install_handler(rustix::process::Signal::WINCH).expect("signal handler install failed");
    SHELL_PID.store(
        rustix::process::getpid().as_raw_pid(),
        std::sync::atomic::Ordering::Relaxed,
    );
    install_handler(rustix::process::Signal::TERM).expect("signal handler install failed");
    install_handler(rustix::process::Signal::HUP).expect("signal handler install failed");

    ignore(rustix::process::Signal::TSTP).expect("signal ignore failed");
    ignore(rustix::process::Signal::TTOU).expect("signal ignore failed");
    ignore(rustix::process::Signal::TTIN).expect("signal ignore failed");
    ignore(rustix::process::Signal::QUIT).expect("signal ignore failed");
    ignore(rustix::process::Signal::PIPE).expect("signal ignore failed");
    ignore(rustix::process::Signal::INT).expect("signal ignore failed");

    install_fatal_handlers();

    read_fd
}

pub fn restore_defaults() -> std::io::Result<()> {
    for &sig in &[
        rustix::process::Signal::INT,
        rustix::process::Signal::QUIT,
        rustix::process::Signal::TSTP,
        rustix::process::Signal::TTOU,
        rustix::process::Signal::TTIN,
        rustix::process::Signal::PIPE,
        rustix::process::Signal::CHILD,
        rustix::process::Signal::WINCH,
    ] {
        set_default(sig)?;
    }
    Ok(())
}

fn set_default(sig: rustix::process::Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        set_linux_disposition(sig, libc::SIG_DFL)
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 0, 0)
    }
}

pub fn read_signal() -> Option<i32> {
    for (sig, bit) in [
        (rustix::process::Signal::INT.as_raw(), 1usize),
        (rustix::process::Signal::TERM.as_raw(), 2),
        (rustix::process::Signal::HUP.as_raw(), 4),
    ] {
        if PENDING_FOREGROUND.fetch_and(!bit, std::sync::atomic::Ordering::AcqRel) & bit != 0 {
            return Some(sig);
        }
    }
    let fd = PIPE_READ.load(std::sync::atomic::Ordering::Acquire);
    if fd < 0 {
        return None;
    }
    let mut byte = 0u8;
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    rustix::io::read(fd, std::slice::from_mut(&mut byte))
        .ok()
        .filter(|&n| n == 1)
        .map(|_| byte as i32)
}

pub(crate) struct ForegroundSignals {
    original_int: libc::sigaction,
    original_tstp: libc::sigaction,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ForegroundSignals {
    pub(crate) fn install() -> std::io::Result<Self> {
        let lock = FOREGROUND_SIGNALS_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("foreground signal state was poisoned"))?;
        let original_int = replace_disposition(
            rustix::process::Signal::INT.as_raw(),
            handler as *const () as usize,
            libc::SA_RESTART,
        )?;
        match replace_disposition(
            rustix::process::Signal::TSTP.as_raw(),
            libc::SIG_IGN,
            0,
        ) {
            Ok(original_tstp) => Ok(Self {
                original_int,
                original_tstp,
                _lock: lock,
            }),
            Err(error) => {
                restore_disposition(rustix::process::Signal::INT.as_raw(), &original_int);
                Err(error)
            }
        }
    }
}

impl Drop for ForegroundSignals {
    fn drop(&mut self) {
        restore_disposition(rustix::process::Signal::INT.as_raw(), &self.original_int);
        restore_disposition(rustix::process::Signal::TSTP.as_raw(), &self.original_tstp);
    }
}

fn replace_disposition(
    sig: i32,
    handler: libc::sighandler_t,
    flags: libc::c_int,
) -> std::io::Result<libc::sigaction> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    action.sa_flags = flags;
    let mask_result = unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if mask_result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut original = std::mem::MaybeUninit::uninit();
    let result = unsafe { libc::sigaction(sig, &action, original.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { original.assume_init() })
}

fn restore_disposition(sig: i32, action: &libc::sigaction) {
    if unsafe { libc::sigaction(sig, action, std::ptr::null_mut()) } != 0 {
        panic!("failed to restore signal {} disposition: {}", sig, std::io::Error::last_os_error());
    }
}

pub(crate) fn note_termination(sig: i32) {
    let _ = PENDING_TERMINATION.compare_exchange(
        0,
        sig,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    );
}

pub fn take_pending_termination() -> Option<i32> {
    match PENDING_TERMINATION.swap(0, std::sync::atomic::Ordering::AcqRel) {
        0 => None,
        sig => Some(sig),
    }
}

fn install_handler(sig: rustix::process::Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::signal(sig.as_raw(), handler as *const () as usize) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), handler as *const () as usize, 0x0002)
    }
}

fn ignore(sig: rustix::process::Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        set_linux_disposition(sig, libc::SIG_IGN)
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 1, 0)
    }
}

/// Install one-shot terminal cleanup for crash signals. A crash while line
/// editing would otherwise leave raw mode, bracketed paste, and a hidden
/// cursor behind in the user's terminal.
fn install_fatal_handlers() {
    for sig in [
        rustix::process::Signal::ABORT,
        rustix::process::Signal::ILL,
        rustix::process::Signal::TRAP,
        rustix::process::Signal::SEGV,
        rustix::process::Signal::BUS,
        rustix::process::Signal::FPE,
    ] {
        install_oneshot_handler(sig, fatal_handler as *const () as usize)
            .expect("fatal signal handler install failed");
    }
}

#[cfg(target_os = "linux")]
fn install_oneshot_handler(
    sig: rustix::process::Signal,
    handler: usize,
) -> std::io::Result<()> {
    // SAFETY: zero initializes the libc action fields before the mask is initialized below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    // One-shot: if the handler itself faults, the second fault dies by the
    // default disposition instead of recursing. The handler exits directly
    // and never depends on the signal mask.
    action.sa_flags = libc::SA_RESETHAND;
    // SAFETY: the mask is a valid mutable libc sigset_t.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the signal and handler use libc's Linux ABI, and sigaction copies the initialized action.
    if unsafe { libc::sigaction(sig.as_raw(), &action, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_oneshot_handler(
    sig: rustix::process::Signal,
    handler: usize,
) -> std::io::Result<()> {
    // 0x0004 is SA_RESETHAND on Darwin: one-shot, matching the Linux
    // install, so a fault inside the handler dies instead of recursing.
    unsafe { darwin_sigaction(sig.as_raw(), handler, 0x0004) }
}

/// Runs on the way to a crash. Restores the terminal, then exits with the
/// conventional 128+signal status, matching the graceful termination path.
/// Exiting directly (instead of re-raising) keeps death deterministic on
/// every platform: re-raising trap- and fault-class signals can wedge the
/// process in the OS crash pipeline instead of terminating it.
unsafe extern "C" fn fatal_handler(sig: i32) {
    unsafe {
        restore_terminal_for_crash();
        libc::_exit(128 + sig);
    }
}

unsafe fn restore_terminal_for_crash() {
    // Best effort and async-signal-safe only: raw descriptor writes, no
    // locks, no allocation.
    // SAFETY: descriptors 0 and 1 stay open for the life of the shell.
    unsafe {
        let stdout = std::os::fd::BorrowedFd::borrow_raw(1);
        let mut remaining = &b"\x1b[?2004l\x1b[?25h"[..];
        while !remaining.is_empty() {
            match rustix::io::write(stdout, remaining) {
                Ok(0) => break,
                Ok(n) => remaining = &remaining[n..],
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => break,
            }
        }
        if let Some(termios) = crate::term::startup_termios() {
            let stdin = std::os::fd::BorrowedFd::borrow_raw(0);
            let _ = rustix::termios::tcsetattr(
                stdin,
                rustix::termios::OptionalActions::Now,
                &termios,
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn set_linux_disposition(
    sig: rustix::process::Signal,
    handler: libc::sighandler_t,
) -> std::io::Result<()> {
    // SAFETY: `sig` is a valid rustix signal and callers pass libc disposition constants.
    let previous = unsafe { libc::signal(sig.as_raw(), handler) };
    if previous == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn errno_slot() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn errno_slot() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

unsafe extern "C" fn handler(sig: i32) {
    let slot = errno_slot();
    let saved = unsafe { *slot };
    unsafe { deliver(sig) };
    unsafe { *slot = saved };
}

unsafe fn deliver(sig: i32) {
    if (sig == libc::SIGTERM || sig == libc::SIGHUP)
        && unsafe { libc::getpid() } != SHELL_PID.load(std::sync::atomic::Ordering::Relaxed)
    {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
        return;
    }
    let pending_bit = match sig {
        libc::SIGINT => 1,
        libc::SIGTERM => {
            note_termination(sig);
            2
        }
        libc::SIGHUP => {
            note_termination(sig);
            4
        }
        _ => 0,
    };
    if pending_bit != 0 {
        PENDING_FOREGROUND.fetch_or(pending_bit, std::sync::atomic::Ordering::Release);
    }
    let byte = sig as u8;
    let fd = PIPE_WRITE.load(std::sync::atomic::Ordering::Acquire);
    if fd >= 0 {
        unsafe {
            let _ = libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinSigaction {
    sa_sigaction: usize,
    sa_mask: u32,
    sa_flags: core::ffi::c_int,
}

#[cfg(target_os = "macos")]
unsafe fn darwin_sigaction(
    signal: i32,
    handler: usize,
    flags: core::ffi::c_int,
) -> std::io::Result<()> {
    unsafe extern "C" {
        fn sigaction(
            signal: core::ffi::c_int,
            action: *const DarwinSigaction,
            old_action: *mut DarwinSigaction,
        ) -> core::ffi::c_int;
    }
    let action = DarwinSigaction {
        sa_sigaction: handler,
        sa_mask: 0,
        sa_flags: flags,
    };
    // SAFETY: the action has the Darwin ABI layout and the API copies it before returning.
    let result = unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn interrupt_is_observed_when_the_signal_pipe_is_full() {
        const CHILD: &str = "ISH_SIGNAL_PIPE_FULL_CHILD";
        if std::env::var_os(CHILD).is_some() {
            init();
            let original_int = read_disposition(libc::SIGINT);
            let original_tstp = read_disposition(libc::SIGTSTP);
            let foreground = ForegroundSignals::install().expect("signal scope installs");
            let fd = PIPE_WRITE.load(std::sync::atomic::Ordering::Acquire);
            let bytes = [0u8; 4096];
            loop {
                let count = unsafe {
                    libc::write(
                        fd,
                        bytes.as_ptr().cast::<libc::c_void>(),
                        bytes.len(),
                    )
                };
                if count < 0 {
                    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN));
                    break;
                }
            }
            unsafe { *errno_slot() = libc::EBADF };
            assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
            assert_eq!(unsafe { *errno_slot() }, libc::EBADF);
            assert_eq!(read_signal(), Some(libc::SIGINT));
            assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
            assert_eq!(read_signal(), Some(libc::SIGTERM));
            assert_eq!(take_pending_termination(), Some(libc::SIGTERM));
            assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
            assert_eq!(read_signal(), Some(libc::SIGHUP));
            assert_eq!(take_pending_termination(), Some(libc::SIGHUP));
            drop(foreground);
            assert_disposition_eq(libc::SIGINT, &original_int);
            assert_disposition_eq(libc::SIGTSTP, &original_tstp);
            return;
        }
        let status = Command::new(std::env::current_exe().expect("test binary is available"))
            .arg("--exact")
            .arg("signal::tests::interrupt_is_observed_when_the_signal_pipe_is_full")
            .env(CHILD, "1")
            .status()
            .expect("signal test child starts");
        assert!(status.success());
    }

    fn read_disposition(sig: i32) -> libc::sigaction {
        let mut action = std::mem::MaybeUninit::uninit();
        // SAFETY: sig is a valid signal and action points to writable storage.
        let result = unsafe { libc::sigaction(sig, std::ptr::null(), action.as_mut_ptr()) };
        assert_eq!(result, 0);
        // SAFETY: sigaction initialized action on success.
        unsafe { action.assume_init() }
    }

    fn assert_disposition_eq(sig: i32, expected: &libc::sigaction) {
        let actual = read_disposition(sig);
        assert_eq!(actual.sa_sigaction, expected.sa_sigaction);
        assert_eq!(actual.sa_flags, expected.sa_flags);
        #[cfg(target_os = "linux")]
        let signal_limit = 64;
        #[cfg(target_os = "macos")]
        let signal_limit = 31;
        for member in 1..=signal_limit {
            // SAFETY: both masks are initialized sigaction values and member is a signal number.
            let actual_member = unsafe { libc::sigismember(&actual.sa_mask, member) };
            // SAFETY: both masks are initialized sigaction values and member is a signal number.
            let expected_member = unsafe { libc::sigismember(&expected.sa_mask, member) };
            assert_eq!(actual_member, expected_member);
        }
    }
}
