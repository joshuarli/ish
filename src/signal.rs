use crate::sys;
use std::os::fd::RawFd;

// SAFETY: These are written once in init() before any signal can fire,
// then only read from handler() (async-signal context) and read_signal()
// (main thread). The shell is single-threaded, so no data race.
static mut PIPE_WRITE: RawFd = -1;
static mut PIPE_READ: RawFd = -1;

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
    unsafe {
        let action = rustix::runtime::KernelSigaction {
            sa_handler_kernel: rustix::runtime::KERNEL_SIG_DFL,
            sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
            sa_restorer: None,
            sa_mask: rustix::runtime::KernelSigSet::empty(),
        };
        let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 0, 0);
    }
}

static INTERRUPT_FLAG: std::sync::atomic::AtomicPtr<std::sync::atomic::AtomicBool> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" fn interrupt_handler(_sig: i32) {
    let flag = INTERRUPT_FLAG.load(std::sync::atomic::Ordering::Relaxed);
    if !flag.is_null() {
        // SAFETY: the flag is a `'static` atomic; storing to it is
        // async-signal-safe.
        unsafe { (*flag).store(true, std::sync::atomic::Ordering::Relaxed) };
    }
}

/// While a native builtin runs in the shell (or a pipeline child forked from
/// it), turn SIGINT into a cancellation flag instead of ignoring it. The
/// handler is installed without `SA_RESTART`, so a blocking prompt read
/// returns `EINTR`.
pub fn catch_interrupts(flag: &'static std::sync::atomic::AtomicBool) {
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    INTERRUPT_FLAG.store(
        flag as *const _ as *mut std::sync::atomic::AtomicBool,
        std::sync::atomic::Ordering::Relaxed,
    );
    #[cfg(target_os = "linux")]
    unsafe {
        // libc `signal` would add SA_RESTART on musl; sigaction with no
        // flags keeps blocking reads interruptible.
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupt_handler as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        let _ = libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(
            rustix::process::Signal::INT.as_raw(),
            interrupt_handler as *const () as usize,
            0,
        );
    }
}

/// Return to ignoring SIGINT after a native builtin.
pub fn ignore_interrupts() {
    ignore(rustix::process::Signal::INT);
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
        // The Rustix kernel_sigaction trampoline segfaults when a signal is
        // delivered to a statically linked musl binary.
        // let action = rustix::runtime::KernelSigaction {
        //     sa_handler_kernel: Some(handler),
        //     sa_flags: rustix::runtime::KernelSigactionFlags::RESTART,
        //     sa_restorer: None,
        //     sa_mask: rustix::runtime::KernelSigSet::empty(),
        // };
        // let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
        let _ = libc::signal(sig.as_raw(), handler as *const () as usize);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), handler as *const () as usize, 0x0002);
    }
}

fn ignore(sig: rustix::process::Signal) {
    #[cfg(target_os = "linux")]
    unsafe {
        let action = rustix::runtime::KernelSigaction {
            sa_handler_kernel: rustix::runtime::kernel_sig_ign(),
            sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
            sa_restorer: None,
            sa_mask: rustix::runtime::KernelSigSet::empty(),
        };
        let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
    }

    #[cfg(target_os = "macos")]
    unsafe {
        darwin_sigaction(sig.as_raw(), 1, 0);
    }
}

unsafe extern "C" fn handler(sig: i32) {
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
