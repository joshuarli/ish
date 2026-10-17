//! Unix process and descriptor operations backed by rustix, with libc at fork,
//! exec, child exit, and raw descriptor boundaries that must accept closed descriptors.

use std::ffi::c_void;
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

pub use libc::{execve, fork};

use rustix::fs::Mode;
use rustix::process::{Pid, WaitOptions};

/// Create a close-on-exec pipe, writing the (read, write) fds into `fds`
/// (length >= 2). Returns 0 on success, -1 on error. Mirrors libc `pipe(int[2])`.
pub fn pipe(fds: *mut i32) -> i32 {
    match pipe_owned() {
        Ok((r, w)) => {
            // SAFETY: caller guarantees fds points to a writable two-element descriptor array.
            unsafe {
                *fds = r.into_raw_fd();
                *fds.add(1) = w.into_raw_fd();
            }
            0
        }
        Err(_) => -1,
    }
}

pub fn pipe_owned() -> std::io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_vendor = "apple")]
    {
        let (read, write) = rustix::pipe::pipe()?;
        rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
        Ok((read, write))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(Into::into)
    }
}

/// Close a descriptor, accepting EBADF for shell redirections that close an already-closed fd.
pub fn close(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: libc close accepts any raw descriptor value and closes only the descriptor named by fd.
    // rustix's raw close assumes fd is valid and panics on EBADF, which shell close redirections allow.
    let result = unsafe { libc::close(fd) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        Ok(())
    } else {
        Err(error)
    }
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

pub fn dup2(oldfd: RawFd, newfd: RawFd) -> i32 {
    match dup2_result(oldfd, newfd) {
        Ok(()) => newfd,
        Err(_) => -1,
    }
}

pub fn dup2_result(oldfd: RawFd, newfd: RawFd) -> std::io::Result<()> {
    // SAFETY: libc dup2 accepts a closed destination and leaves descriptor ownership with the caller.
    let result = unsafe { libc::dup2(oldfd, newfd) };
    if result == newfd {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn duplicate_fd_above(fd: RawFd, min_fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: fcntl accepts raw shell descriptor numbers and reports EBADF when fd is closed.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min_fd) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a newly allocated open descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

pub fn descriptor_is_open(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl accepts raw shell descriptor numbers and reports EBADF when fd is closed.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
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
        let _ = close(fds[0]);
        let _ = close(fds[1]);

        assert!(read_flags.contains(rustix::io::FdFlags::CLOEXEC));
        assert!(write_flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}
