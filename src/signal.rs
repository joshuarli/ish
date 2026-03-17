use std::os::fd::RawFd;

static mut PIPE_WRITE: RawFd = -1;
static mut PIPE_READ: RawFd = -1;

/// Initialize signal handling: create self-pipe, install handlers.
/// Returns the read-end fd for polling.
pub fn init() -> RawFd {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");

    for &fd in &fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }

    unsafe {
        PIPE_READ = fds[0];
        PIPE_WRITE = fds[1];
    }

    install_handler(libc::SIGCHLD);
    install_handler(libc::SIGWINCH);

    // Shell ignores these — it must not be suspended/stopped
    ignore(libc::SIGTSTP);
    ignore(libc::SIGTTOU);
    ignore(libc::SIGTTIN);
    ignore(libc::SIGQUIT);
    ignore(libc::SIGPIPE);
    // SIGINT: we're in raw mode (ISIG off) at the prompt so we get 0x03 as a byte.
    // During child exec, the child's pgid is foreground so SIGINT goes there.
    // Ignore SIGINT for the shell itself as a safety net.
    ignore(libc::SIGINT);

    unsafe { PIPE_READ }
}

/// Restore default signal dispositions — called in child after fork, before exec.
pub fn restore_defaults() {
    for &sig in &[
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTOU,
        libc::SIGTTIN,
        libc::SIGPIPE,
        libc::SIGCHLD,
        libc::SIGWINCH,
    ] {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
        }
    }
}

/// Read one signal byte from the self-pipe. Returns the signal number or None.
pub fn read_signal() -> Option<i32> {
    let mut byte = 0u8;
    let n = unsafe { libc::read(PIPE_READ, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    if n == 1 {
        Some(byte as i32)
    } else {
        None
    }
}

/// Drain all pending signals from the pipe.
pub fn drain() {
    while read_signal().is_some() {}
}

fn install_handler(sig: i32) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

fn ignore(sig: i32) {
    unsafe {
        libc::signal(sig, libc::SIG_IGN);
    }
}

extern "C" fn handler(sig: i32) {
    let byte = sig as u8;
    unsafe {
        libc::write(PIPE_WRITE, &byte as *const u8 as *const libc::c_void, 1);
    }
}
