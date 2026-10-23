//! Platform-specific syscall wrappers.

use std::os::fd::{IntoRawFd, OwnedFd, RawFd};

/// Create a pipe with O_CLOEXEC on both ends.
/// Descriptor flags are set after creation and are not atomic with fork.
pub fn pipe_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = pipe_cloexec_owned()?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}

pub(crate) fn pipe_cloexec_owned() -> Result<(OwnedFd, OwnedFd), std::io::Error> {
    let (read, write) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
    Ok((read, write))
}

/// Create a pipe with O_CLOEXEC | O_NONBLOCK on both ends.
/// Descriptor flags are set after creation and are not atomic with fork.
pub fn pipe_nonblock_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = pipe_cloexec_owned()?;
    rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK)?;
    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}
