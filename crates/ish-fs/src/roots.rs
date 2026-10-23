use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, Mode, OFlags};

pub(crate) use fdu_posix::identity::{EntryIdentity as Identity, EntryType as EntryKind};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Timestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Observation {
    pub identity: Identity,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub links: u64,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
}

impl Observation {
    pub fn same_version(self, actual: Self) -> bool {
        let mut expected = self;
        expected.atime = actual.atime;
        expected == actual
    }

    #[allow(clippy::unnecessary_cast, reason = "stat field widths differ between Linux and Darwin")]
    fn from_stat(stat: &fs::Stat) -> io::Result<Self> {
        Ok(Self {
            identity: fdu_posix::identity::entry_identity(stat)?,
            size: stat.st_size.max(0) as u64,
            mode: stat.st_mode as u32,
            uid: stat.st_uid,
            gid: stat.st_gid,
            links: fdu_posix::identity::link_count(stat)?,
            atime: Timestamp { seconds: stat.st_atime as i64, nanoseconds: stat.st_atime_nsec as u32 },
            mtime: Timestamp { seconds: stat.st_mtime as i64, nanoseconds: stat.st_mtime_nsec as u32 },
            ctime: Timestamp { seconds: stat.st_ctime as i64, nanoseconds: stat.st_ctime_nsec as u32 },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Requirement {
    Existing,
    MayBeAbsent,
}

#[derive(Debug)]
pub(crate) struct Root {
    pub parent: OwnedFd,
    pub name: OsString,
    pub observed: Option<Observation>,
    pub path: PathBuf,
}

impl Root {
    pub fn establish(cwd: BorrowedFd<'_>, path: &Path, requirement: Requirement) -> io::Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "path is empty or contains NUL"));
        }
        let last = bytes.iter().rposition(|byte| *byte != b'/');
        let (parent_path, name) = match last {
            None => (Path::new("/"), OsStr::new(".")),
            Some(last) => {
                let stripped = &bytes[..=last];
                match stripped.iter().rposition(|byte| *byte == b'/') {
                    Some(0) => (Path::new("/"), OsStr::from_bytes(&stripped[1..])),
                    Some(slash) => (Path::new(OsStr::from_bytes(&stripped[..slash])), OsStr::from_bytes(&stripped[slash + 1..])),
                    None => (Path::new("."), OsStr::from_bytes(stripped)),
                }
            }
        };
        let parent = fs::openat(cwd, parent_path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Self::from_parent(parent, name, path, requirement)
    }

    pub fn establish_at(parent: BorrowedFd<'_>, name: &OsStr, path: &Path, requirement: Requirement) -> io::Result<Self> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry name is not a single path component"));
        }
        let parent = fs::openat(parent, ".", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Self::from_parent(parent, name, path, requirement)
    }

    fn from_parent(parent: OwnedFd, name: &OsStr, path: &Path, requirement: Requirement) -> io::Result<Self> {
        let observed = match observe(parent.as_fd(), name) {
            Ok(observed) => Some(observed),
            Err(error) if error.kind() == io::ErrorKind::NotFound && requirement == Requirement::MayBeAbsent => None,
            Err(error) => return Err(error),
        };
        Ok(Self { parent, name: name.to_os_string(), observed, path: path.to_path_buf() })
    }

    pub fn open_directory(&self) -> io::Result<OwnedFd> {
        let expected = self.observed.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "directory root is absent"))?;
        if expected.identity.kind != EntryKind::Directory {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "root is not a directory"));
        }
        let fd = fs::openat(&self.parent, &self.name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW, Mode::empty())?;
        if observe_fd(fd.as_fd())?.identity != expected.identity {
            return Err(io::Error::other("directory identity changed"));
        }
        Ok(fd)
    }

    pub fn open_directory_following_final(&self) -> io::Result<OwnedFd> {
        let expected = self.observed.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "directory root is absent"))?;
        if !matches!(expected.identity.kind, EntryKind::Directory | EntryKind::Symlink) {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "root is not a directory"));
        }
        let fd = fs::openat(&self.parent, &self.name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        let opened = observe_fd(fd.as_fd())?;
        if expected.identity.kind == EntryKind::Directory {
            if opened.identity != expected.identity {
                return Err(io::Error::other("directory identity changed"));
            }
        } else if observe(self.parent.as_fd(), &self.name)?.identity != expected.identity {
            return Err(io::Error::other("directory symlink identity changed"));
        }
        Ok(fd)
    }

    pub fn parent_identity(&self) -> io::Result<Identity> {
        observe_fd(self.parent.as_fd()).map(|observed| observed.identity)
    }

    pub fn is_mount_root(&self) -> io::Result<bool> {
        if self.observed.is_none_or(|observed| observed.identity.kind != EntryKind::Directory) {
            return Ok(false);
        }
        let directory = self.open_directory()?;
        let mount = mount_identity(directory.as_fd())?;
        let parent = mount_identity(self.parent.as_fd())?;
        Ok(mount != parent)
    }

    pub fn same_entry(&self, other: &Self) -> io::Result<bool> {
        Ok(self.name == other.name && self.parent_identity()? == other.parent_identity()?)
    }

    pub fn is_ancestor_of(&self, candidate: &Self) -> io::Result<bool> {
        let Some(ancestor) = self.observed else {
            return Ok(false);
        };
        if ancestor.identity.kind != EntryKind::Directory {
            return Ok(false);
        }
        if candidate.observed.is_some_and(|observed| observed.identity == ancestor.identity) {
            return Ok(true);
        }
        candidate.parent_chain_contains(|identity| identity == ancestor.identity)
    }

    pub fn is_ancestor_of_fd(&self, candidate: BorrowedFd<'_>) -> io::Result<bool> {
        let Some(ancestor) = self.observed else {
            return Ok(false);
        };
        if ancestor.identity.kind != EntryKind::Directory {
            return Ok(false);
        }
        if observe_fd(candidate)?.identity == ancestor.identity {
            return Ok(true);
        }
        directory_chain_contains(candidate, |identity| identity == ancestor.identity)
    }

    pub fn parent_chain_contains(&self, predicate: impl FnMut(Identity) -> bool) -> io::Result<bool> {
        directory_chain_contains(self.parent.as_fd(), predicate)
    }
}

fn directory_chain_contains(
    start: BorrowedFd<'_>,
    mut predicate: impl FnMut(Identity) -> bool,
) -> io::Result<bool> {
    let mut current = fs::openat(start, ".", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    loop {
        let current_identity = observe_fd(current.as_fd())?.identity;
        if predicate(current_identity) {
            return Ok(true);
        }
        let parent = fs::openat(current.as_fd(), "..", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        let parent_identity = observe_fd(parent.as_fd())?.identity;
        if parent_identity == current_identity {
            return Ok(false);
        }
        current = parent;
    }
}

pub(crate) fn observe(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<Observation> {
    Observation::from_stat(&fdu_posix::identity::stat_at(parent, name)?)
}

pub(crate) fn observe_fd(fd: BorrowedFd<'_>) -> io::Result<Observation> {
    Observation::from_stat(&fdu_posix::identity::retry(|| fs::fstat(fd))?)
}

pub(crate) use fdu_posix::identity::mount_identity;
