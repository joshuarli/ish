//! Platform filesystem primitives.
//!
//! Everything here is descriptor-relative and synchronous. rustix covers the
//! system calls; `libc` is used only for `fchflags` on macOS, which rustix does
//! not expose.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags, Timestamps};
use rustix::io::Errno;

use crate::fault;

/// An error's description without the `(os error N)` suffix.
pub fn describe_error(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Convert raw path bytes to a C string, rejecting interior NULs.
pub fn cstr(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub fn os_cstr(s: &OsStr) -> io::Result<CString> {
    cstr(s.as_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    CharDev,
    BlockDev,
    Unknown,
}

impl Kind {
    pub fn code(self) -> u8 {
        match self {
            Kind::File => 1,
            Kind::Dir => 2,
            Kind::Symlink => 3,
            Kind::Fifo => 4,
            Kind::Socket => 5,
            Kind::CharDev => 6,
            Kind::BlockDev => 7,
            Kind::Unknown => 0,
        }
    }

    pub fn from_code(code: u8) -> Kind {
        match code {
            1 => Kind::File,
            2 => Kind::Dir,
            3 => Kind::Symlink,
            4 => Kind::Fifo,
            5 => Kind::Socket,
            6 => Kind::CharDev,
            7 => Kind::BlockDev,
            _ => Kind::Unknown,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "directory",
            Kind::Symlink => "symlink",
            Kind::Fifo => "fifo",
            Kind::Socket => "socket",
            Kind::CharDev => "character device",
            Kind::BlockDev => "block device",
            Kind::Unknown => "unknown",
        }
    }
}

/// The subset of `stat` that recovery decisions use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: (i64, u32),
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
    pub flags: u32,
}

impl Stat {
    // Field types differ between macOS and Linux, so some casts are no-ops
    // on one platform.
    #[allow(clippy::unnecessary_cast)]
    fn from_raw(st: &rustix::fs::Stat) -> Stat {
        let kind = match FileType::from_raw_mode(st.st_mode as _) {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Dir,
            FileType::Symlink => Kind::Symlink,
            FileType::Fifo => Kind::Fifo,
            FileType::Socket => Kind::Socket,
            FileType::CharacterDevice => Kind::CharDev,
            FileType::BlockDevice => Kind::BlockDev,
            FileType::Unknown => Kind::Unknown,
        };
        #[cfg(target_vendor = "apple")]
        let flags = st.st_flags;
        #[cfg(not(target_vendor = "apple"))]
        let flags = 0;
        Stat {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            kind,
            mode: st.st_mode as u32 & 0o7777,
            uid: st.st_uid,
            gid: st.st_gid,
            nlink: st.st_nlink as u64,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            atime: (st.st_atime as i64, st.st_atime_nsec as u32),
            mtime: (st.st_mtime as i64, st.st_mtime_nsec as u32),
            ctime: (st.st_ctime as i64, st.st_ctime_nsec as u32),
            flags,
        }
    }

    pub fn same_object(&self, other: &Stat) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }

    /// Everything the kernel changes on a content or metadata write. Equality
    /// is evidence of an unchanged object, not proof of equal contents.
    pub fn unchanged_since(&self, earlier: &Stat) -> bool {
        self.same_object(earlier)
            && self.kind == earlier.kind
            && self.size == earlier.size
            && self.mtime == earlier.mtime
            && self.ctime == earlier.ctime
    }
}

pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    Ok(Stat::from_raw(&rustix::fs::fstat(fd)?))
}

pub fn lstat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Stat> {
    Ok(Stat::from_raw(&rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

pub fn lstat(path: &Path) -> io::Result<Stat> {
    Ok(Stat::from_raw(&rustix::fs::statat(
        rustix::fs::CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

pub fn stat(path: &Path) -> io::Result<Stat> {
    Ok(Stat::from_raw(&rustix::fs::statat(
        rustix::fs::CWD,
        path,
        AtFlags::empty(),
    )?))
}

pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Open a directory entry without following a final symlink.
pub fn open_dir_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Open an entry for reading without following symlinks or blocking on FIFOs.
pub fn open_read_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub fn create_excl_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode as _),
    )?)
}

pub fn read_link_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(dir, name, Vec::new())?.into_bytes())
}

/// Split an absolute or relative path into its parent directory and final
/// component. `..`, `.`, and empty final components are rejected.
pub fn split_parent(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no final component")
    })?;
    let parent = match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => Path::new("/"),
    };
    Ok((parent, name))
}

/// Lexically clean an absolute path: drop `.` components and empty segments,
/// but keep `..` because symlinks make it non-lexical.
pub fn clean_path(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether an error means the filesystem lacks a capability, as opposed to a
/// permission, I/O, or space failure that must be reported as such.
pub fn is_capability_error(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == libc::ENOTSUP
            || code == libc::EOPNOTSUPP
            || code == libc::ENOTTY
            || code == libc::EXDEV
            || code == libc::ENOSYS
            || code == libc::EINVAL
    ) || err.kind() == io::ErrorKind::Unsupported
}

/// Clone `src` to a new entry `dst_name` in `dst_dir`.
///
/// macOS uses `fclonefileat`; Linux creates the destination and uses the
/// `FICLONE` ioctl. The clone shares blocks copy-on-write, so later writes to
/// the source do not change it.
pub fn clone_file(src: BorrowedFd<'_>, dst_dir: BorrowedFd<'_>, dst_name: &CStr) -> io::Result<()> {
    fault::check("clone")?;
    #[cfg(target_vendor = "apple")]
    {
        rustix::fs::fclonefileat(src, dst_dir, dst_name, rustix::fs::CloneFlags::empty())?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let dst = create_excl_at(dst_dir, dst_name, 0o600)?;
        match rustix::fs::ioctl_ficlone(&dst, src) {
            Ok(()) => Ok(()),
            Err(e) => {
                drop(dst);
                let _ = rustix::fs::unlinkat(dst_dir, dst_name, AtFlags::empty());
                Err(e.into())
            }
        }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        let _ = (src, dst_dir, dst_name);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Hard-link an entry without following symlinks.
pub fn link_at(
    src_dir: BorrowedFd<'_>,
    src_name: &CStr,
    dst_dir: BorrowedFd<'_>,
    dst_name: &CStr,
) -> io::Result<()> {
    fault::check("link")?;
    Ok(rustix::fs::linkat(
        src_dir,
        src_name,
        dst_dir,
        dst_name,
        AtFlags::empty(),
    )?)
}

pub fn unlink_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    fault::check("unlink")?;
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
}

pub fn rmdir_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    fault::check("rmdir")?;
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

pub fn mkdir_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::mkdirat(
        dir,
        name,
        Mode::from_raw_mode(mode as _),
    )?)
}

pub fn symlink_at(target: &[u8], dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    Ok(rustix::fs::symlinkat(cstr(target)?.as_c_str(), dir, name)?)
}

/// Rename that fails with `EEXIST` instead of replacing an existing entry.
pub fn rename_noreplace(
    from_dir: BorrowedFd<'_>,
    from: &CStr,
    to_dir: BorrowedFd<'_>,
    to: &CStr,
) -> io::Result<()> {
    fault::check("rename")?;
    match rustix::fs::renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Filesystems without no-replace support: check and rename. The
        // remaining race is reported by the caller's revalidation.
        Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::NOTSUP) => match lstat_at(to_dir, to) {
            Ok(_) => Err(io::Error::from_raw_os_error(libc::EEXIST)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(rustix::fs::renameat(from_dir, from, to_dir, to)?)
            }
            Err(e) => Err(e),
        },
        Err(e) => Err(e.into()),
    }
}

/// Rename that atomically replaces an existing non-directory destination.
pub fn rename_replace(
    from_dir: BorrowedFd<'_>,
    from: &CStr,
    to_dir: BorrowedFd<'_>,
    to: &CStr,
) -> io::Result<()> {
    fault::check("rename")?;
    Ok(rustix::fs::renameat(from_dir, from, to_dir, to)?)
}

pub fn sync_file(fd: BorrowedFd<'_>) -> io::Result<()> {
    fault::check("sync")?;
    Ok(rustix::fs::fsync(fd)?)
}

/// Flush a directory so entries created in it survive an OS crash.
pub fn sync_dir(fd: BorrowedFd<'_>) -> io::Result<()> {
    fault::check("sync")?;
    match rustix::fs::fsync(fd) {
        Ok(()) => Ok(()),
        // Some filesystems reject fsync on directory descriptors.
        Err(Errno::INVAL) | Err(Errno::BADF) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn free_bytes(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let st = rustix::fs::fstatvfs(fd)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Filesystem type name, for inspection output only. Never used as proof of
/// a capability.
pub fn fs_type_name(fd: BorrowedFd<'_>) -> String {
    #[cfg(target_vendor = "apple")]
    {
        match rustix::fs::fstatfs(fd) {
            Ok(st) => {
                let raw: Vec<u8> = st
                    .f_fstypename
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8)
                    .collect();
                String::from_utf8_lossy(&raw).into_owned()
            }
            Err(_) => "unknown".into(),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match rustix::fs::fstatfs(fd) {
            Ok(st) => match st.f_type as u64 {
                0xEF53 => "ext4".into(),
                0x9123683E => "btrfs".into(),
                0x58465342 => "xfs".into(),
                0x01021994 => "tmpfs".into(),
                0x794C7630 => "overlayfs".into(),
                0x2FC12FC1 => "zfs".into(),
                0xF2F52010 => "f2fs".into(),
                0x6969 => "nfs".into(),
                0x65735546 => "fuse".into(),
                other => format!("0x{other:x}"),
            },
            Err(_) => "unknown".into(),
        }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        let _ = fd;
        "unknown".into()
    }
}

/// Byte-copy accounting shared by an operation.
#[derive(Default, Debug)]
pub struct CopyStats {
    /// Data bytes read and written in userspace.
    pub bytes: u64,
}

/// Copy file data from `src` to `dst` in userspace, skipping holes where the
/// platform reports them. Stops with `Interrupted` when `cancel` is set.
pub fn copy_data(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    size: u64,
    cancel: &AtomicBool,
    stats: &mut CopyStats,
) -> io::Result<()> {
    use rustix::fs::SeekFrom;
    fault::check("copy")?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut offset = 0u64;
    while offset < size {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        // Find the next data region; filesystems without hole reporting
        // treat the whole file as data.
        let data_start = match rustix::fs::seek(src, SeekFrom::Data(offset)) {
            Ok(pos) => pos,
            Err(Errno::NXIO) => break,
            Err(_) => offset,
        };
        let data_end = match rustix::fs::seek(src, SeekFrom::Hole(data_start)) {
            Ok(pos) => pos.min(size),
            Err(_) => size,
        };
        let mut pos = data_start;
        while pos < data_end {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let want = ((data_end - pos) as usize).min(buf.len());
            let n = rustix::io::pread(src, &mut buf[..want], pos)?;
            if n == 0 {
                break;
            }
            let mut written = 0;
            while written < n {
                fault::check("write")?;
                let w = rustix::io::pwrite(dst, &buf[written..n], pos + written as u64)?;
                if w == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                written += w;
            }
            pos += n as u64;
            stats.bytes += n as u64;
        }
        offset = data_end.max(offset + 1).max(pos);
    }
    rustix::fs::ftruncate(dst, size)?;
    Ok(())
}

/// Compare two files' contents. Stops early on the first difference.
pub fn same_contents(
    a: BorrowedFd<'_>,
    b: BorrowedFd<'_>,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let (sa, sb) = (fstat(a)?, fstat(b)?);
    if sa.size != sb.size {
        return Ok(false);
    }
    let mut ba = vec![0u8; 128 * 1024];
    let mut bb = vec![0u8; 128 * 1024];
    let mut pos = 0u64;
    while pos < sa.size {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let want = ((sa.size - pos) as usize).min(ba.len());
        let na = read_full_at(a, &mut ba[..want], pos)?;
        let nb = read_full_at(b, &mut bb[..want], pos)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            break;
        }
        pos += na as u64;
    }
    Ok(true)
}

pub fn read_full_at(fd: BorrowedFd<'_>, buf: &mut [u8], mut pos: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = rustix::io::pread(fd, &mut buf[filled..], pos)?;
        if n == 0 {
            break;
        }
        filled += n;
        pos += n as u64;
    }
    Ok(filled)
}

/// Extended attributes as `(name, value)` pairs.
pub type Xattrs = Vec<(Vec<u8>, Vec<u8>)>;

/// Extended attributes of an open object, bounded by `limit` total bytes;
/// `Ok(None)` means the limit was exceeded.
pub fn read_xattrs(fd: BorrowedFd<'_>, limit: usize) -> io::Result<Option<Xattrs>> {
    let mut empty: [u8; 0] = [];
    let size = match rustix::fs::flistxattr(fd, &mut empty[..]) {
        Ok(n) => n,
        Err(Errno::NOTSUP) | Err(Errno::NOSYS) => return Ok(Some(Vec::new())),
        Err(e) => return Err(e.into()),
    };
    if size == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut names = vec![0u8; size];
    let n = rustix::fs::flistxattr(fd, &mut names[..])?;
    names.truncate(n);
    let mut out = Vec::new();
    let mut total = 0usize;
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let cname = cstr(name)?;
        let len = match rustix::fs::fgetxattr(fd, cname.as_c_str(), &mut empty[..]) {
            Ok(len) => len,
            Err(Errno::NODATA) => continue,
            Err(e) => return Err(e.into()),
        };
        total += name.len() + len;
        if total > limit {
            return Ok(None);
        }
        let mut value = vec![0u8; len];
        let got = rustix::fs::fgetxattr(fd, cname.as_c_str(), &mut value[..])?;
        value.truncate(got);
        out.push((name.to_vec(), value));
    }
    Ok(Some(out))
}

/// Set extended attributes. Returns names that could not be restored.
pub fn write_xattrs(fd: BorrowedFd<'_>, xattrs: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
    let mut failed = Vec::new();
    for (name, value) in xattrs {
        let ok = cstr(name).is_ok_and(|cname| {
            rustix::fs::fsetxattr(fd, cname.as_c_str(), value, rustix::fs::XattrFlags::empty())
                .is_ok()
        });
        if !ok {
            failed.push(name.clone());
        }
    }
    failed
}

/// Copy extended attributes, ACLs, and (on macOS) resource forks from one
/// open file to another. Returns a fidelity note when something was lost.
pub fn copy_file_metadata(src: BorrowedFd<'_>, dst: BorrowedFd<'_>) -> Option<String> {
    #[cfg(target_vendor = "apple")]
    {
        // fcopyfile carries ACLs and resource forks, which the xattr
        // interface does not expose completely.
        use rustix::fs::{CopyfileFlags, copyfile_state_alloc, copyfile_state_free, fcopyfile};
        let state = match copyfile_state_alloc() {
            Ok(state) => state,
            Err(e) => return Some(format!("metadata copy unavailable: {e}")),
        };
        // SAFETY: `state` was allocated above and is freed exactly once below.
        let result =
            unsafe { fcopyfile(src, dst, state, CopyfileFlags::ACL | CopyfileFlags::XATTR) };
        // SAFETY: allocated by copyfile_state_alloc and not used afterward.
        let _ = unsafe { copyfile_state_free(state) };
        result
            .err()
            .map(|e| format!("extended attributes or ACL not copied: {e}"))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        match read_xattrs(src, 16 * 1024 * 1024) {
            Ok(Some(xattrs)) => {
                let failed = write_xattrs(dst, &xattrs);
                (!failed.is_empty()).then(|| {
                    let names: Vec<String> = failed
                        .iter()
                        .map(|n| String::from_utf8_lossy(n).into_owned())
                        .collect();
                    format!("extended attributes not restored: {}", names.join(", "))
                })
            }
            Ok(None) => Some("extended attributes exceed the copy limit".into()),
            Err(e) => Some(format!("extended attributes not read: {e}")),
        }
    }
}

pub fn set_mode(fd: BorrowedFd<'_>, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::fchmod(fd, Mode::from_raw_mode(mode as _))?)
}

pub fn set_times(fd: BorrowedFd<'_>, atime: (i64, u32), mtime: (i64, u32)) -> io::Result<()> {
    let ts = |(sec, nsec): (i64, u32)| rustix::fs::Timespec {
        tv_sec: sec as _,
        tv_nsec: nsec as _,
    };
    Ok(rustix::fs::futimens(
        fd,
        &Timestamps {
            last_access: ts(atime),
            last_modification: ts(mtime),
        },
    )?)
}

pub fn set_symlink_times(
    dir: BorrowedFd<'_>,
    name: &CStr,
    atime: (i64, u32),
    mtime: (i64, u32),
) -> io::Result<()> {
    let ts = |(sec, nsec): (i64, u32)| rustix::fs::Timespec {
        tv_sec: sec as _,
        tv_nsec: nsec as _,
    };
    Ok(rustix::fs::utimensat(
        dir,
        name,
        &Timestamps {
            last_access: ts(atime),
            last_modification: ts(mtime),
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

/// Set ownership when it differs. Unprivileged callers can usually only
/// change the group, so failure is reported rather than treated as fatal.
pub fn set_owner(fd: BorrowedFd<'_>, uid: u32, gid: u32) -> io::Result<()> {
    let current = fstat(fd)?;
    if current.uid == uid && current.gid == gid {
        return Ok(());
    }
    let owner = (current.uid != uid).then(|| rustix::fs::Uid::from_raw(uid));
    let group = (current.gid != gid).then(|| rustix::fs::Gid::from_raw(gid));
    Ok(rustix::fs::fchown(fd, owner, group)?)
}

/// BSD file flags (`chflags`). rustix has no `fchflags`, so this is a narrow
/// libc call on macOS and a no-op elsewhere.
pub fn set_flags(fd: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fd` is a valid open descriptor for the duration of the call.
        if unsafe { libc::fchflags(fd.as_raw_fd(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = (fd, flags);
        Ok(())
    }
}

/// Current wall-clock time as (seconds, nanoseconds).
pub fn now() -> (i64, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos())
}

pub fn now_ns() -> u64 {
    let (s, n) = now();
    (s as u64) * 1_000_000_000 + n as u64
}

/// A random 64-bit value from the OS, for session and object identifiers.
pub fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    let read = rustix::fs::open(
        "/dev/urandom",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .and_then(|fd| rustix::io::read(&fd, &mut buf));
    if read == Ok(8) {
        return u64::from_le_bytes(buf);
    }
    // Identifiers only need to be unique, so a time/pid mix is an acceptable
    // fallback when the random device is unavailable.
    now_ns() ^ ((rustix::process::getpid().as_raw_nonzero().get() as u64) << 40)
}

/// Remove the entry `name` in `dir` and everything below it, without
/// following symlinks and without crossing into other filesystems. Only
/// used for trees ish itself owns (store directories, unpublished staging).
pub fn remove_tree(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    fn remove(dir: BorrowedFd<'_>, name: &CStr, dev: u64, depth: usize) -> io::Result<()> {
        let st = match lstat_at(dir, name) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if st.kind != Kind::Dir {
            return match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(e.into()),
            };
        }
        if st.dev != dev || depth > 1024 {
            return Err(io::Error::other(
                "refusing to remove across a mount or too deep",
            ));
        }
        let fd = open_dir_at(dir, name)?;
        let mut d = rustix::fs::Dir::read_from(&fd)?;
        let mut names = Vec::new();
        while let Some(entry) = d.read() {
            let entry = entry?;
            let n = entry.file_name();
            if n != c"." && n != c".." {
                names.push(n.to_owned());
            }
        }
        for child in names {
            remove(fd.as_fd(), &child, dev, depth + 1)?;
        }
        match rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    let dev = fstat(dir)?.dev;
    remove(dir, name, dev, 0)
}
