//! Platform-specific syscall wrappers.

use std::os::fd::{IntoRawFd, RawFd};

/// Create a pipe with O_CLOEXEC on both ends.
/// 3 syscalls: pipe + 2x fcntl (not atomic with respect to fork).
pub fn pipe_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}

/// Create a pipe with O_CLOEXEC | O_NONBLOCK on both ends.
/// Used for the signal self-pipe. The flags are set with `fcntl` after
/// `pipe`, so they are not atomic with respect to fork; the shell creates
/// the pipe once at startup, before any child exists.
pub fn pipe_nonblock_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK)?;
    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}
