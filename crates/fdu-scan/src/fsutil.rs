

use fdu_core::{EntryType, FileIdentity, ModifiedTime};
use rustix::fs::{self, Mode, Stat};
use rustix::io::Errno;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use std::ffi::CStr;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Mutex, TryLockError};

pub(crate) use fdu_posix::identity::{DIRECTORY_FLAGS, identity, entry_type, link_count, retry};

static FILE_LIMIT_LOCK: Mutex<()> = Mutex::new(());


fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}



pub(crate) fn file_size(stat: &Stat, apparent: bool) -> io::Result<u64> {
    if apparent {
        u64::try_from(stat.st_size).map_err(|_| invalid("file size is outside the supported byte-count range"))
    } else {
        u64::try_from(stat.st_blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(512))
            .ok_or_else(|| invalid("allocated file size is outside the supported byte-count range"))
    }
}

#[allow(clippy::useless_conversion)]
fn modified_time(stat: &Stat) -> io::Result<ModifiedTime> {
    let seconds = i64::try_from(stat.st_mtime).map_err(|_| invalid("modification time is outside the supported range"))?;
    Ok(ModifiedTime::from_unix_seconds(seconds).unwrap_or_default())
}



pub(crate) struct EntryStat {
    pub(crate) identity: FileIdentity,
    pub(crate) modified_time: ModifiedTime,
    pub(crate) link_count: u64,
    pub(crate) entry_type: EntryType,
    pub(crate) apparent_bytes: io::Result<u64>,
    pub(crate) allocated_bytes: io::Result<u64>,
}

pub(crate) fn stat_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<Stat> {
    fdu_posix::identity::stat_at(parent, name).map_err(Into::into)
}

pub(crate) fn stat_entry_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
    let stat = stat_at(parent, name)?;
    Ok(EntryStat {
        identity: identity(&stat)?,
        modified_time: modified_time(&stat)?,
        link_count: link_count(&stat)?,
        entry_type: entry_type(&stat),
        apparent_bytes: file_size(&stat, true),
        allocated_bytes: file_size(&stat, false),
    })
}

pub(crate) fn identity_and_link_count(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, u64)> {
    let stat = retry(|| fs::fstat(fd))?;
    Ok((identity(&stat)?, link_count(&stat)?))
}

#[cfg(target_os = "macos")]
pub(crate) fn identity_link_count_and_modified_time(
    fd: BorrowedFd<'_>,
) -> io::Result<(FileIdentity, u64, ModifiedTime)> {
    let stat = retry(|| fs::fstat(fd))?;
    Ok((identity(&stat)?, link_count(&stat)?, modified_time(&stat)?))
}

pub(crate) fn identity_of(fd: BorrowedFd<'_>) -> io::Result<FileIdentity> {
    identity_and_link_count(fd).map(|(identity, _)| identity)
}



pub(crate) fn with_descriptor_retry(mut open: impl FnMut() -> io::Result<OwnedFd>) -> io::Result<OwnedFd> {
    loop {
        match open() {
            Err(error) if error.raw_os_error() == Some(Errno::MFILE.raw_os_error()) => {
                raise_soft_nofile_limit()?;
            }
            result => return result,
        }
    }
}


pub(crate) fn open_directory_at(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg + Copy,
) -> io::Result<OwnedFd> {
    with_descriptor_retry(|| retry(|| fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())))
}

pub(crate) fn open_directory_no_follow(path: &std::path::Path) -> io::Result<OwnedFd> {
    retry(|| fs::open(path, DIRECTORY_FLAGS, Mode::empty()))
}

fn raise_soft_nofile_limit() -> io::Result<()> {
    let _guard = match FILE_LIMIT_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            drop(FILE_LIMIT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            return Ok(());
        }
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
    };
    let limit = getrlimit(Resource::Nofile);
    let (Some(current), maximum) = (limit.current, limit.maximum) else {
        return Err(io::Error::other("process file descriptor limit is unlimited yet exhausted"));
    };
    if maximum.is_some_and(|maximum| current >= maximum) {
        return Err(io::Error::other("process reached its hard file descriptor limit"));
    }
    let mut next = current.saturating_add(current.max(256));
    if let Some(maximum) = maximum {
        next = next.min(maximum);
    }
    if next <= current {
        return Err(io::Error::other("process file descriptor limit cannot be increased"));
    }
    setrlimit(Resource::Nofile, Rlimit { current: Some(next), maximum })?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn raise_descriptor_limit(cap: u64) -> usize {
    let limit = getrlimit(Resource::Nofile);
    let target = limit.maximum.map_or(cap, |maximum| maximum.min(cap));
    let mut current = limit.current;
    if current.is_none_or(|current| current < target)
        && setrlimit(Resource::Nofile, Rlimit { current: Some(target), maximum: limit.maximum }).is_ok()
    {
        current = Some(target);
    }
    current.map_or(usize::MAX, |current| usize::try_from(current).unwrap_or(usize::MAX))
}

