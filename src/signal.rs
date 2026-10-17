use crate::sys;
use std::os::fd::RawFd;

// SAFETY: These are written once in init() before any signal can fire,
// then only read from handler() (async-signal context) and read_signal()
// (main thread). The shell is single-threaded, so no data race.
static mut PIPE_WRITE: RawFd = -1;
static mut PIPE_READ: RawFd = -1;
static SHELL_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Initialize signal handling: create self-pipe, install handlers.
/// Returns the read-end fd for polling.
pub fn init() -> RawFd {
    let (read_fd, write_fd) =
        sys::pipe_nonblock_cloexec().expect("pipe() failed for signal self-pipe");

    // SAFETY: Called once at startup before any signals are installed.
    // Single-threaded — no concurrent access.
    unsafe {
        PIPE_READ = read_fd;
        PIPE_WRITE = write_fd;
    }

    // External jobs are waited synchronously, so SIGCHLD does not need to wake
    // the input poller. The old rustix self-pipe handler caused the shell to
    // segfault after child exit on Alpine/musl.
    // install_handler(rustix::process::Signal::CHILD);
    install_handler(rustix::process::Signal::WINCH);
    // TERM and HUP reach the prompt as events, so the shell restores the
    // terminal and ends any suspended job before exiting.
    SHELL_PID.store(
        rustix::process::getpid().as_raw_pid(),
        std::sync::atomic::Ordering::Relaxed,
    );
    install_handler(rustix::process::Signal::TERM);
    install_handler(rustix::process::Signal::HUP);

    // Shell ignores these — it must not be suspended/stopped
    ignore(rustix::process::Signal::TSTP);
    ignore(rustix::process::Signal::TTOU);
    ignore(rustix::process::Signal::TTIN);
    ignore(rustix::process::Signal::QUIT);
    ignore(rustix::process::Signal::PIPE);
    // SIGINT: we're in raw mode (ISIG off) at the prompt so we get 0x03 as a byte.
    // During child exec, the child's pgid is foreground so SIGINT goes there.
    // Ignore SIGINT for the shell itself as a safety net.
    ignore(rustix::process::Signal::INT);

    // SAFETY: PIPE_READ was set above, single-threaded.
    unsafe { PIPE_READ }
}

/// Restore default signal dispositions — called in child after fork, before exec.
pub fn restore_defaults() {
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
        set_default(sig);
    }
}

fn set_default(sig: rustix::process::Signal) {
    #[cfg(target_os = "linux")]
    {
        set_linux_disposition(sig, libc::SIG_DFL);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 0, 0);
    }
}

/// Read one signal byte from the self-pipe. Returns the signal number or None.
pub fn read_signal() -> Option<i32> {
    let mut byte = 0u8;
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(PIPE_READ) };
    rustix::io::read(fd, std::slice::from_mut(&mut byte))
        .ok()
        .filter(|&n| n == 1)
        .map(|_| byte as i32)
}

fn install_handler(sig: rustix::process::Signal) {
    #[cfg(target_os = "linux")]
    unsafe {
        // rustix's kernel_sigaction trampoline segfaults when a signal is
        // delivered to a statically linked musl binary, so handlers are
        // installed through libc::signal instead.
        let _ = libc::signal(sig.as_raw(), handler as *const () as usize);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), handler as *const () as usize, 0x0002);
    }
}

fn ignore(sig: rustix::process::Signal) {
    #[cfg(target_os = "linux")]
    {
        set_linux_disposition(sig, libc::SIG_IGN);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 1, 0);
    }
}

#[cfg(target_os = "linux")]
fn set_linux_disposition(sig: rustix::process::Signal, handler: libc::sighandler_t) {
    // SAFETY: `sig` is a valid rustix signal and callers pass libc disposition constants.
    let previous = unsafe { libc::signal(sig.as_raw(), handler) };
    if previous == libc::SIG_ERR {
        let error = std::io::Error::last_os_error();
        panic!("failed to set signal {} disposition: {error}", sig.as_raw());
    }
}

unsafe extern "C" fn handler(sig: i32) {
    // A process forked from the shell without exec (a pipeline stage) inherits
    // this handler. TERM and HUP must still end it: restore the default
    // action and re-raise instead of queueing the signal for the shell.
    // getpid, signal, and raise are async-signal-safe.
    if (sig == libc::SIGTERM || sig == libc::SIGHUP)
        && unsafe { libc::getpid() } != SHELL_PID.load(std::sync::atomic::Ordering::Relaxed)
    {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
        return;
    }
    let byte = sig as u8;
    // SAFETY: write() is async-signal-safe. PIPE_WRITE is valid (set in init).
    // O_NONBLOCK ensures we never block in the handler. If the pipe is full,
    // the signal was already pending — dropping the byte is safe.
    unsafe {
        let _ = libc::write(PIPE_WRITE, &byte as *const u8 as *const libc::c_void, 1);
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
unsafe fn darwin_sigaction(signal: i32, handler: usize, flags: core::ffi::c_int) {
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
    let _ = unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
}
