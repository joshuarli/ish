use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, Mode, OFlags};

use super::records::Records;
use crate::roots::{self, Identity, Observation};

pub(super) const DIRECTORY: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
pub(super) const READ_FILE: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);

pub(super) fn unchanged(expected: Observation, actual: Observation) -> bool {
    let mut expected = expected;
    expected.atime = actual.atime;
    expected == actual
}

pub(super) fn require_unchanged(parent: BorrowedFd<'_>, name: &OsStr, expected: Observation) -> io::Result<()> {
    if unchanged(expected, roots::observe(parent, name)?) { Ok(()) } else { Err(io::Error::other("source identity or version changed")) }
}

pub(super) fn identity(parent: BorrowedFd<'_>, name: &OsStr, expected: Identity) -> io::Result<()> {
    if roots::observe(parent, name)?.identity == expected { Ok(()) } else { Err(io::Error::other("directory entry identity changed")) }
}

pub(super) fn parent(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<(OwnedFd, OsString)> {
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no final name"))?.to_os_string();
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    Ok((open_directory(root, directory, directories, destination)?, name))
}

pub(super) fn open_directory(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<OwnedFd> {
    let mut fd = fs::openat(root, ".", DIRECTORY, Mode::empty())?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else { return Err(io::Error::new(io::ErrorKind::InvalidData, "non-relative traversal record")); };
        prefix.push(name);
        let expected = directories.lock().map_err(|_| io::Error::other("directory records poisoned"))?.find(&prefix)?.ok_or_else(|| io::Error::other("missing directory identity record"))?;
        let next = fs::openat(fd.as_fd(), name, DIRECTORY, Mode::empty())?;
        let actual = roots::observe_fd(next.as_fd())?.identity;
        let expected = if destination { expected.published } else { expected.observed.identity };
        if actual != expected { return Err(io::Error::other("traversal directory identity changed")); }
        fd = next;
    }
    Ok(fd)
}
