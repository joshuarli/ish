//! Unix process and descriptor operations backed by rustix, with libc at fork,
//! exec, child exit, and raw descriptor boundaries that must accept closed descriptors.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

pub use libc::{execve, fork};

use rustix::fs::Mode;

pub fn pipe_owned() -> std::io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_vendor = "apple")]
    let (read, write) = {
        let (read, write) = rustix::pipe::pipe()?;
        rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
        (read, write)
    };
    #[cfg(not(target_vendor = "apple"))]
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let read = if read.as_raw_fd() < 3 { duplicate_fd_above(read.as_raw_fd(), 3)? } else { read };
    let write = if write.as_raw_fd() < 3 { duplicate_fd_above(write.as_raw_fd(), 3)? } else { write };
    Ok((read, write))
}

pub fn heredoc_file() -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::memfd_create("epsh-heredoc", rustix::fs::MemfdFlags::CLOEXEC)
            .map(std::fs::File::from).map_err(Into::into)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir();
        for _ in 0..128 {
            let path = directory.join(format!("epsh-heredoc-{}-{}", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed)));
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
                Ok(file) => {
                    std::fs::remove_file(path)?;
                    return Ok(file);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "cannot allocate heredoc storage"))
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
    descriptor_flags(fd).map(|_| ())
}

pub fn descriptor_flags(fd: RawFd) -> std::io::Result<rustix::io::FdFlags> {
    // SAFETY: fcntl accepts raw shell descriptor numbers and reports EBADF when fd is closed.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rustix::io::FdFlags::from_bits_retain(flags as _))
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

/// Return the real user ID of the calling process.
pub fn getuid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Set the process file-mode creation mask. Returns the previous mask.
pub fn umask(mask: u32) -> u32 {
    let previous_mask = rustix::process::umask(Mode::from_bits_retain(mask as _)).bits();
    #[cfg(target_vendor = "apple")]
    let previous_mask = u32::from(previous_mask);
    previous_mask
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
        let (read, write) = pipe_owned().unwrap();
        let read_flags = rustix::io::fcntl_getfd(&read).expect("read end flags");
        let write_flags = rustix::io::fcntl_getfd(&write).expect("write end flags");

        assert!(read_flags.contains(rustix::io::FdFlags::CLOEXEC));
        assert!(write_flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}
