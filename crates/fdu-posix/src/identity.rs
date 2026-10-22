use std::io;
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
pub use fdu_core::{EntryType, FileIdentity};

pub const READ_FILE_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);

pub const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EntryIdentity {
    pub file: FileIdentity,
    pub kind: EntryType,
}

impl std::ops::Deref for EntryIdentity {
    type Target = FileIdentity;
    fn deref(&self) -> &FileIdentity { &self.file }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountIdentity {
    pub device: u64,
    pub mount_id: u64,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountIdentity {
    pub device: u64,
    pub fsid: [i32; 2],
    pub mount_point: Vec<u8>,
}

pub fn retry_errno<T>(mut operation: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
    loop {
        match operation() {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

pub fn retry<T>(operation: impl FnMut() -> rustix::io::Result<T>) -> io::Result<T> {
    retry_errno(operation).map_err(Into::into)
}

#[allow(clippy::useless_conversion)]
pub fn identity(metadata: &Stat) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        device: u64::try_from(metadata.st_dev).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device number"))?,
        inode: metadata.st_ino,
    })
}

#[allow(clippy::unnecessary_fallible_conversions, clippy::useless_conversion)]
pub fn link_count(metadata: &Stat) -> io::Result<u64> {
    u64::try_from(metadata.st_nlink).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid link count"))
}

pub fn entry_type(metadata: &Stat) -> EntryType {
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::Directory => EntryType::Directory,
        FileType::RegularFile => EntryType::RegularFile,
        FileType::Symlink => EntryType::Symlink,
        _ => EntryType::Other,
    }
}

pub fn entry_identity(metadata: &Stat) -> io::Result<EntryIdentity> {
    Ok(EntryIdentity { file: identity(metadata)?, kind: entry_type(metadata) })
}

pub fn stat_at(parent: BorrowedFd<'_>, name: impl rustix::path::Arg + Copy) -> rustix::io::Result<Stat> {
    retry_errno(|| fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
}

pub fn open_no_follow(parent: BorrowedFd<'_>, name: &std::ffi::OsStr, kind: EntryType) -> io::Result<OwnedFd> {
    let flags = if kind == EntryType::Symlink {
        #[cfg(target_os = "linux")]
        { OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC }
        #[cfg(target_os = "macos")]
        { OFlags::from_bits_retain(libc::O_SYMLINK as u32) | OFlags::RDONLY | OFlags::CLOEXEC }
    } else { READ_FILE_FLAGS };
    retry(|| fs::openat(parent, name, flags, Mode::empty()))
}

pub fn mount_identity(fd: BorrowedFd<'_>) -> io::Result<MountIdentity> {
    directory_state(fd).map(|(_, mount)| mount)
}

#[cfg(target_os = "macos")]
pub fn directory_state(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, MountIdentity)> {
    let metadata = retry(|| fs::fstat(fd))?;
    let filesystem = retry(|| fs::fstatfs(fd))?;

    assert_eq!(std::mem::size_of_val(&filesystem.f_fsid), std::mem::size_of::<[i32; 2]>());
    // SAFETY: macOS defines fsid_t as two i32 words; both representation sizes were checked.
    let fsid = unsafe { std::mem::transmute_copy::<_, [i32; 2]>(&filesystem.f_fsid) };
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    let identity = identity(&metadata)?;
    Ok((identity, MountIdentity { device: identity.device, fsid, mount_point }))
}

#[cfg(target_os = "linux")]
pub fn directory_state(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, MountIdentity)> {
    let wanted = fs::StatxFlags::INO | fs::StatxFlags::MNT_ID;
    let metadata = retry(|| fs::statx(fd, c"", AtFlags::EMPTY_PATH, wanted))?;
    if !fs::StatxFlags::from_bits_retain(metadata.stx_mask).contains(wanted) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report the directory identity and mount ID",
        ));
    }
    let device = fs::makedev(metadata.stx_dev_major, metadata.stx_dev_minor);
    Ok((
        FileIdentity { device, inode: metadata.stx_ino },
        MountIdentity { device, mount_id: metadata.stx_mnt_id },
    ))
}
