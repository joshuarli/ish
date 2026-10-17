//! Thin wrappers around syscalls, backed by rustix where possible.
//!
//! `fork` and `execve` have no rustix equivalent and remain thin libc wrappers.
//! Everything else is routed through rustix (`process`, `io`, `pipe`, `termios`,
//! `fs`, `runtime` features).

use std::ffi::c_void;
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

// `fork` and `execve` have no rustix equivalent — keep libc wrappers.
pub use libc::{execve, fork};

use rustix::fs::Mode;
use rustix::process::{Pid, WaitOptions};

/// Create a close-on-exec pipe, writing the (read, write) fds into `fds`
/// (length >= 2). Returns 0 on success, -1 on error. Mirrors libc `pipe(int[2])`.
pub fn pipe(fds: *mut i32) -> i32 {
    #[cfg(target_vendor = "apple")]
    let result = rustix::pipe::pipe();
    #[cfg(not(target_vendor = "apple"))]
    let result = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC);

    match result {
        Ok((r, w)) => {
            // SAFETY: caller guarantees `fds` points to a writable [i32; 2].
            let read_fd = r.into_raw_fd();
            let write_fd = w.into_raw_fd();
            #[cfg(target_vendor = "apple")]
            if rustix::io::fcntl_setfd(
                unsafe { BorrowedFd::borrow_raw(read_fd) },
                rustix::io::FdFlags::CLOEXEC,
            )
            .and(rustix::io::fcntl_setfd(
                unsafe { BorrowedFd::borrow_raw(write_fd) },
                rustix::io::FdFlags::CLOEXEC,
            ))
            .is_err()
            {
                close(read_fd);
                close(write_fd);
                return -1;
            }
            unsafe {
                *fds = read_fd;
                *fds.add(1) = write_fd;
            }
            0
        }
        Err(_) => -1,
    }
}

/// Close a file descriptor (ignores EBADF, matching the libc close semantics
/// used by callers that may close an already-closed fd).
pub fn close(fd: RawFd) {
    // SAFETY: fd is a raw fd; rustix closes it. Invalid fds return EBADF,
    // which we intentionally ignore.
    unsafe { rustix::io::close(fd) };
}

/// Read up to `len` bytes from `fd` into `buf`. Returns bytes read (>= 0)
/// or -1 on error. Mirrors libc `read`.
pub fn read(fd: RawFd, buf: *mut c_void, len: usize) -> isize {
    // SAFETY: caller guarantees `buf` is valid for `len` bytes.
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match rustix::io::read(unsafe { BorrowedFd::borrow_raw(fd) }, slice) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

/// Write `len` bytes from `buf` to `fd`. Mirrors libc `write`.
pub fn write(fd: RawFd, buf: *const u8, len: usize) {
    // SAFETY: caller guarantees `buf` is valid for `len` bytes.
    let slice = unsafe { std::slice::from_raw_parts(buf, len) };
    let _ = rustix::io::write(unsafe { BorrowedFd::borrow_raw(fd) }, slice);
}

/// Duplicate a file descriptor. Mirrors libc `dup2`: makes `newfd` a copy of
/// `oldfd`, closing `newfd` first if open. Returns newfd on success, -1 on error.
pub fn dup2(oldfd: RawFd, newfd: RawFd) -> i32 {
    // SAFETY: standard dup2. We take ownership of newfd via an OwnedFd, then
    // leak it with `forget` after the call, matching the pattern used by
    // `rustix::stdio::dup2_stdin` and similar rustix helpers.
    unsafe {
        let mut new = OwnedFd::from_raw_fd(newfd);
        let result = rustix::io::dup2(BorrowedFd::borrow_raw(oldfd), &mut new);
        // Prevent Drop from closing newfd regardless of success/failure.
        std::mem::forget(new);
        match result {
            Ok(()) => newfd,
            Err(_) => -1,
        }
    }
}

/// Wait for a child process. Mirrors libc `waitpid`: returns the pid (or -1 on
/// error), and writes the raw status word into `status`. `options` is 0 or
/// `WUNTRACED` (nonzero).
pub fn waitpid(pid: i32, status: &mut i32, options: i32) -> i32 {
    // libc `waitpid(-1, ...)` = wait for any child.
    let pid_opt = if pid < 0 { None } else { Pid::from_raw(pid) };
    let opts = if options != 0 {
        WaitOptions::UNTRACED
    } else {
        WaitOptions::empty()
    };
    match rustix::process::waitpid(pid_opt, opts) {
        // No status available (e.g. WNOHANG semantics) — report 0 like libc.
        Ok(None) => 0,
        Ok(Some((p, st))) => {
            *status = st.as_raw();
            p.as_raw_pid()
        }
        Err(_) => -1,
    }
}

/// True if `status` indicates normal exit (WIFEXITED).
pub fn wifexited(status: i32) -> bool {
    (status & 0x7f) == 0
}

/// Extract the exit code from `status` (valid only if `wifexited`).
pub fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

/// Extract the terminating signal from `status` (valid only if not exited).
pub fn wtermsig(status: i32) -> i32 {
    status & 0x7f
}

/// True if `status` indicates the process was stopped (WIFSTOPPED / WUNTRACED).
pub fn wifstopped(status: i32) -> bool {
    (status & 0xff) == 0x7f
}

/// Return the real user ID of the calling process.
pub fn getuid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Set the process file-mode creation mask. Returns the previous mask.
pub fn umask(mask: u32) -> u32 {
    rustix::process::umask(Mode::from_bits_retain(mask as _))
        .bits()
        .into()
}

/// Exit the process, flushing Rust's stdout first.
/// The caller flushes stdout before calling so buffered output is not lost
/// when fd 1 is a pipe (e.g. in command substitution or pipelines).
pub fn exit_child(status: crate::error::ExitStatus) -> ! {
    {
        let mut out = std::io::stdout().lock();
        let _ = std::io::Write::flush(&mut out);
    }
    // SAFETY: _exit is always safe to call; it terminates the process immediately.
    // `rustix::runtime::exit_group` is linux_raw-only, so use libc here for
    // cross-platform parity with the original `_exit` semantics (no atexit flush).
    unsafe { libc::_exit(status.code()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_fds_are_close_on_exec() {
        let mut fds = [-1; 2];
        assert_eq!(pipe(fds.as_mut_ptr()), 0);

        let read_flags = rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(fds[0]) })
            .expect("read end flags");
        let write_flags = rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(fds[1]) })
            .expect("write end flags");
        close(fds[0]);
        close(fds[1]);

        assert!(read_flags.contains(rustix::io::FdFlags::CLOEXEC));
        assert!(write_flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}
