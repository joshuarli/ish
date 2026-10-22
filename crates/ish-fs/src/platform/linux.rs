#[cfg(test)]
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use rustix::fs::{self, AtFlags, IFlags, SeekFrom, Timespec, Timestamps};
use rustix::io::Errno;

#[cfg(test)]
use super::metadata::ALLOCATION_OVERHEAD as XATTR_ALLOCATION_OVERHEAD;
use super::visit_nonzero_ranges;
use super::metadata::{CaptureBudget as MetadataCaptureBudget, allocation_error, too_large as metadata_too_large};

use crate::Cancellation;
use crate::platform::{
    AccelerationError, CopyMethod, CopyStats, ExtendedMetadata, MetadataTarget, Preservation,
    Xattr, METADATA_CAPTURE_BYTES,
};
use crate::roots::{self, Observation, Timestamp};

const COPY_RANGE_CHUNK: usize = 8 * 1024 * 1024;
const XATTR_CAPTURE_ATTEMPTS: usize = 8;
const POSIX_ACL_ACCESS: &[u8] = b"system.posix_acl_access";
const SECURITY_CAPABILITY: &[u8] = b"security.capability";
#[cfg(target_arch = "aarch64")]
// The asm-generic ABI assigns fchmodat2 452; pinned libc omits its aarch64 binding.
const FCHMODAT2_SYSCALL: libc::c_long = 452;
#[cfg(not(target_arch = "aarch64"))]
const FCHMODAT2_SYSCALL: libc::c_long = libc::SYS_fchmodat2;
pub(super) const BLOCKING_FLAGS: u32 = IFlags::APPEND.bits() | IFlags::IMMUTABLE.bits();
const STRUCTURAL_FLAGS: u32 = 0x0000_0100
    | 0x0000_0200
    | 0x0000_1000
    | 0x0000_2000
    | 0x0004_0000
    | 0x0008_0000
    | 0x0020_0000
    | 0x0040_0000;
const KNOWN_MUTABLE_FLAGS: u32 = IFlags::APPEND.bits()
    | IFlags::COMPRESSED.bits()
    | IFlags::DIRSYNC.bits()
    | IFlags::IMMUTABLE.bits()
    | IFlags::JOURNALING.bits()
    | IFlags::NOATIME.bits()
    | IFlags::NOCOW.bits()
    | IFlags::NODUMP.bits()
    | IFlags::NOTAIL.bits()
    | IFlags::PROJECT_INHERIT.bits()
    | IFlags::SECURE_REMOVAL.bits()
    | IFlags::SYNC.bits()
    | IFlags::TOPDIR.bits()
    | IFlags::UNRM.bits();

#[cfg(test)]
thread_local! {
    static TEST_DENSE_DESTINATION: Cell<bool> = const { Cell::new(false) };
    static TEST_SKIP_REFLINK: Cell<bool> = const { Cell::new(false) };
    static TEST_SPARSE_SEEK_UNSUPPORTED: Cell<bool> = const { Cell::new(false) };
    static TEST_COPY_RANGE_UNSUPPORTED_AFTER: Cell<Option<u64>> = const { Cell::new(None) };
    static TEST_COPY_RANGE_FAULT_TRIGGERED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn force_dense_destination_for_test() {
    TEST_DENSE_DESTINATION.with(|force| force.set(true));
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn skip_reflink_for_test(skip: bool) {
    TEST_SKIP_REFLINK.with(|value| value.set(skip));
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn make_sparse_seek_unsupported_for_test(unsupported: bool) {
    TEST_SPARSE_SEEK_UNSUPPORTED.with(|value| value.set(unsupported));
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn fail_copy_range_after_for_test(bytes: Option<u64>) {
    TEST_COPY_RANGE_UNSUPPORTED_AFTER.with(|value| value.set(bytes));
    TEST_COPY_RANGE_FAULT_TRIGGERED.with(|value| value.set(false));
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn copy_range_fault_triggered_for_test() -> bool {
    TEST_COPY_RANGE_FAULT_TRIGGERED.with(Cell::get)
}

pub(crate) fn try_reflink(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
) -> Result<(), AccelerationError> {
    #[cfg(test)]
    if TEST_SKIP_REFLINK.with(Cell::get) {
        return Err(AccelerationError::Unsupported);
    }

    loop {
        match fs::ioctl_ficlone(dst, src) {
            Ok(()) => return Ok(()),
            Err(Errno::INTR) => continue,
            Err(error) if reflink_unsupported(error) => {
                return Err(AccelerationError::Unsupported);
            }
            Err(error) => return Err(AccelerationError::Io(error)),
        }
    }
}

pub(crate) fn prepare_directory(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: roots::Identity,
) -> io::Result<OwnedFd> {
    let name_bytes = name.as_bytes();
    if name_bytes.is_empty()
        || name_bytes.contains(&0)
        || name_bytes.contains(&b'/')
        || name_bytes == b"."
        || name_bytes == b".."
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging directory name is not a single path component",
        ));
    }
    if expected.kind != roots::EntryKind::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected staging entry is not a directory",
        ));
    }

    let path_fd = fs::openat(
        parent,
        name,
        rustix::fs::OFlags::PATH
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        fs::Mode::empty(),
    )?;
    verify_directory_identity(path_fd.as_fd(), expected)?;
    chmod_held_directory(parent, path_fd.as_fd())?;

    let opened = fs::openat(
        path_fd.as_fd(),
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC,
        fs::Mode::empty(),
    )?;
    verify_directory_identity(opened.as_fd(), expected)?;
    let mode = fs::fstat(opened.as_fd())?.st_mode as u32;
    if mode & 0o7777 != 0o700 {
        return Err(io::Error::other(
            "prepared staging directory permissions changed",
        ));
    }
    Ok(opened)
}

fn verify_directory_identity(fd: BorrowedFd<'_>, expected: roots::Identity) -> io::Result<()> {
    let observed = roots::observe_fd(fd)?;
    if observed.identity != expected || observed.identity.kind != roots::EntryKind::Directory {
        return Err(io::Error::other("staging directory identity changed"));
    }
    Ok(())
}

fn chmod_held_directory(parent: BorrowedFd<'_>, fd: BorrowedFd<'_>) -> io::Result<()> {
    loop {
        // SAFETY: the descriptor stays borrowed for the syscall, and an empty path with
        // AT_EMPTY_PATH applies chmod to that held descriptor.
        let result = unsafe {
            libc::syscall(
                FCHMODAT2_SYSCALL,
                fd.as_raw_fd(),
            c"".as_ptr(),
                0o700 as libc::mode_t,
                libc::AT_EMPTY_PATH,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(error);
        }

        let proc_fd_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        return fs::chmodat(
            parent,
            proc_fd_path.as_str(),
            fs::Mode::from_raw_mode(0o700),
            AtFlags::empty(),
        )
        .map_err(Into::into);
    }
}

pub(crate) fn copy_data(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<CopyStats> {
    let source_offset = seek_retry(src, SeekFrom::Current(0))?;
    let result = copy_data_with_source_offset(src, dst, length, scratch, cancel);
    let restore = seek_retry(src, SeekFrom::Start(source_offset));
    match (result, restore) {
        (Ok(stats), Ok(_)) => {
            #[cfg(test)]
            if TEST_DENSE_DESTINATION.with(|force| force.replace(false)) && length != 0 {
                fs::fallocate(dst, fs::FallocateFlags::empty(), 0, length)?;
            }
            Ok(stats)
        },
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(restore_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; restoring the source offset also failed: {restore_error}"),
        )),
    }
}

fn copy_data_with_source_offset(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<CopyStats> {
    if scratch.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy buffer is empty",
        ));
    }
    check_cancelled(cancel)?;

    let source_stat = fs::fstat(src)?;
    let destination_stat = fs::fstat(dst)?;
    if source_stat.st_dev == destination_stat.st_dev && source_stat.st_ino == destination_stat.st_ino
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination refer to the same file",
        ));
    }
    if source_stat.st_size < 0 || (source_stat.st_size as u64) < length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source is shorter than the captured length",
        ));
    }

    if length == 0 {
        fs::ftruncate(dst, 0)?;
        return Ok(CopyStats {
            method: CopyMethod::Native,
            logical_bytes: 0,
            streamed_bytes: 0,
        });
    }

    match try_reflink(src, dst) {
        Ok(()) => {
            check_cancelled(cancel)?;
            let cloned_size = fs::fstat(dst)?.st_size;
            if cloned_size < 0 || (cloned_size as u64) < length {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "reflink source ended before the captured length",
                ));
            }
            fs::ftruncate(dst, length)?;
            return Ok(CopyStats {
                method: CopyMethod::Clone,
                logical_bytes: length,
                streamed_bytes: 0,
            });
        }
        Err(AccelerationError::Unsupported) => fs::ftruncate(dst, 0)?,
        Err(AccelerationError::Io(error)) => return Err(error.into()),
    }

    match copy_sparse_native(src, dst, length, cancel) {
        Ok(streamed_bytes) => {
            verify_source_length(src, length)?;
            fs::ftruncate(dst, length)?;
            return Ok(CopyStats {
                method: CopyMethod::Native,
                logical_bytes: length,
                streamed_bytes,
            });
        }
        Err(NativeCopyError::Unsupported) => fs::ftruncate(dst, 0)?,
        Err(NativeCopyError::Io(error)) => return Err(error),
    }

    let streamed_bytes = match copy_sparse_buffered(src, dst, length, scratch, cancel) {
        Ok(bytes) => bytes,
        Err(SparseCopyError::Unsupported) => {
            fs::ftruncate(dst, 0)?;
            copy_zero_scanning(src, dst, length, scratch, cancel)?
        }
        Err(SparseCopyError::Io(error)) => return Err(error),
    };
    verify_source_length(src, length)?;
    fs::ftruncate(dst, length)?;
    Ok(CopyStats {
        method: CopyMethod::Buffered,
        logical_bytes: length,
        streamed_bytes,
    })
}

fn verify_source_length(src: BorrowedFd<'_>, length: u64) -> io::Result<()> {
    let stat = fs::fstat(src)?;
    if stat.st_size < 0 || (stat.st_size as u64) < length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source is shorter than the captured length",
        ));
    }
    Ok(())
}

fn seek_retry(fd: BorrowedFd<'_>, position: SeekFrom) -> rustix::io::Result<u64> {
    loop {
        match fs::seek(fd, position) {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

enum NativeCopyError {
    Unsupported,
    Io(io::Error),
}

enum SparseCopyError {
    Unsupported,
    Io(io::Error),
}

fn copy_sparse_native(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    cancel: &Cancellation,
) -> Result<u64, NativeCopyError> {
    let mut offset = 0;
    let mut streamed = 0;
    while offset < length {
        check_cancelled(cancel).map_err(NativeCopyError::Io)?;
        let data = match seek_sparse(src, SeekFrom::Data(offset)) {
            Ok(data) => data,
            Err(SparseSeekError::End) => break,
            Err(SparseSeekError::Unsupported) => return Err(NativeCopyError::Unsupported),
            Err(SparseSeekError::Io(error)) => return Err(NativeCopyError::Io(error)),
        };
        if data >= length {
            break;
        }
        let hole = match seek_sparse(src, SeekFrom::Hole(data)) {
            Ok(hole) => hole.min(length),
            Err(SparseSeekError::End) => length,
            Err(SparseSeekError::Unsupported) => return Err(NativeCopyError::Unsupported),
            Err(SparseSeekError::Io(error)) => return Err(NativeCopyError::Io(error)),
        };
        if hole <= data {
            return Err(NativeCopyError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse extent did not advance",
            )));
        }
        let mut input_offset = data;
        let mut output_offset = data;
        while output_offset < hole {
            check_cancelled(cancel).map_err(NativeCopyError::Io)?;
            let count = usize::try_from((hole - output_offset).min(COPY_RANGE_CHUNK as u64))
                .expect("copy range is bounded by its chunk size");
            let result = copy_file_range(
                src,
                &mut input_offset,
                dst,
                &mut output_offset,
                count,
                streamed,
            );
            match result {
                Ok(0) => {
                    return Err(NativeCopyError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "source ended before the captured length",
                    )));
                }
                Ok(copied) => streamed += copied as u64,
                Err(error) if cfr_unsupported(error) => {
                    return Err(NativeCopyError::Unsupported);
                }
                Err(error) => return Err(NativeCopyError::Io(error.into())),
            }
        }
        offset = hole;
    }
    Ok(streamed)
}

fn copy_file_range(
    src: BorrowedFd<'_>,
    input_offset: &mut u64,
    dst: BorrowedFd<'_>,
    output_offset: &mut u64,
    count: usize,
    _already_copied: u64,
) -> Result<usize, Errno> {
    #[cfg(test)]
    if TEST_COPY_RANGE_UNSUPPORTED_AFTER.with(|limit| {
        limit
            .get()
            .is_some_and(|limit| _already_copied >= limit)
    }) {
        TEST_COPY_RANGE_FAULT_TRIGGERED.with(|value| value.set(true));
        return Err(Errno::NOTSUP);
    }

    loop {
        match fs::copy_file_range(
            src,
            Some(input_offset),
            dst,
            Some(output_offset),
            count,
        ) {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

enum SparseSeekError {
    End,
    Unsupported,
    Io(io::Error),
}

fn seek_sparse(fd: BorrowedFd<'_>, position: SeekFrom) -> Result<u64, SparseSeekError> {
    #[cfg(test)]
    if TEST_SPARSE_SEEK_UNSUPPORTED.with(Cell::get) {
        return Err(SparseSeekError::Unsupported);
    }

    loop {
        match fs::seek(fd, position) {
            Ok(offset) => return Ok(offset),
            Err(Errno::NXIO) => return Err(SparseSeekError::End),
            Err(error) if sparse_seek_unsupported(error) => {
                return Err(SparseSeekError::Unsupported);
            }
            Err(Errno::INTR) => continue,
            Err(error) => return Err(SparseSeekError::Io(error.into())),
        }
    }
}

fn copy_sparse_buffered(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> Result<u64, SparseCopyError> {
    let mut offset = 0;
    let mut streamed = 0;
    while offset < length {
        check_cancelled(cancel).map_err(SparseCopyError::Io)?;
        let data = match seek_sparse(src, SeekFrom::Data(offset)) {
            Ok(data) => data,
            Err(SparseSeekError::End) => break,
            Err(SparseSeekError::Unsupported) => return Err(SparseCopyError::Unsupported),
            Err(SparseSeekError::Io(error)) => return Err(SparseCopyError::Io(error)),
        };
        if data >= length {
            break;
        }
        let hole = match seek_sparse(src, SeekFrom::Hole(data)) {
            Ok(hole) => hole.min(length),
            Err(SparseSeekError::End) => length,
            Err(SparseSeekError::Unsupported) => return Err(SparseCopyError::Unsupported),
            Err(SparseSeekError::Io(error)) => return Err(SparseCopyError::Io(error)),
        };
        if hole <= data {
            return Err(SparseCopyError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse extent did not advance",
            )));
        }
        streamed += copy_buffered_range(src, dst, data, hole, scratch, cancel)
            .map_err(SparseCopyError::Io)?;
        offset = hole;
    }
    Ok(streamed)
}

fn copy_zero_scanning(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<u64> {
    let mut offset = 0;
    let mut streamed = 0;
    while offset < length {
        check_cancelled(cancel)?;
        let wanted = usize::try_from((length - offset).min(scratch.len() as u64))
            .expect("buffered read is bounded by its buffer");
        let read = pread_retry(src, &mut scratch[..wanted], offset, cancel)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source ended before the captured length",
            ));
        }
        streamed += read as u64;
        visit_nonzero_ranges(&scratch[..read], |range| {
            pwrite_all(
                dst,
                &scratch[range.clone()],
                offset + range.start as u64,
                cancel,
            )
        })?;
        offset += read as u64;
    }
    Ok(streamed)
}

fn copy_buffered_range(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<u64> {
    let mut offset = start;
    while offset < end {
        check_cancelled(cancel)?;
        let wanted = usize::try_from((end - offset).min(scratch.len() as u64))
            .expect("buffered read is bounded by its buffer");
        let read = pread_retry(src, &mut scratch[..wanted], offset, cancel)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source ended before the captured length",
            ));
        }
        pwrite_all(dst, &scratch[..read], offset, cancel)?;
        offset += read as u64;
    }
    Ok(end - start)
}

fn pread_retry(
    fd: BorrowedFd<'_>,
    buffer: &mut [u8],
    offset: u64,
    cancel: &Cancellation,
) -> io::Result<usize> {
    loop {
        check_cancelled(cancel)?;
        match rustix::io::pread(fd, &mut *buffer, offset) {
            Err(Errno::INTR) => continue,
            result => return result.map_err(Into::into),
        }
    }
}

fn pwrite_all(
    fd: BorrowedFd<'_>,
    mut buffer: &[u8],
    mut offset: u64,
    cancel: &Cancellation,
) -> io::Result<()> {
    while !buffer.is_empty() {
        check_cancelled(cancel)?;
        match rustix::io::pwrite(fd, buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "destination write made no progress",
                ));
            }
            Ok(written) => {
                buffer = &buffer[written..];
                offset += written as u64;
            }
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn capture_metadata(target: MetadataTarget<'_>) -> io::Result<ExtendedMetadata> {
    match target {
        MetadataTarget::Fd(fd) => {
            let xattrs = read_fd_xattrs(fd)?;
            let flags = capture_flags(fd)?;
            Ok(ExtendedMetadata {
                xattrs: xattrs.0,
                acl: xattrs.1,
                flags,
            })
        }
        MetadataTarget::Symlink { parent, name } => {
            let before = roots::observe(parent, name)?;
            let path = proc_fd_child(parent, name)?;
            let xattrs = read_path_xattrs(&path)?;
            let after = roots::observe(parent, name)?;
            ensure_stable_link_metadata(before, after)?;
            Ok(ExtendedMetadata {
                xattrs: xattrs.0,
                acl: xattrs.1,
                flags: 0,
            })
        }
    }
}

fn read_fd_xattrs(fd: BorrowedFd<'_>) -> io::Result<(Vec<Xattr>, Option<Vec<u8>>)> {
    read_xattrs(
        |buffer| fs::flistxattr(fd, buffer),
        |name, buffer| fs::fgetxattr(fd, name, buffer),
    )
}

fn read_path_xattrs(path: &OsStr) -> io::Result<(Vec<Xattr>, Option<Vec<u8>>)> {
    read_xattrs(
        |buffer| fs::llistxattr(path, buffer),
        |name, buffer| fs::lgetxattr(path, name, buffer),
    )
}

fn read_xattrs(
    list: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    get: impl FnMut(&OsStr, &mut [u8]) -> rustix::io::Result<usize>,
) -> io::Result<(Vec<Xattr>, Option<Vec<u8>>)> {
    let mut budget = MetadataCaptureBudget::new(METADATA_CAPTURE_BYTES);
    read_xattrs_with_budget(list, get, &mut budget)
}

fn read_xattrs_with_budget(
    mut list: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    mut get: impl FnMut(&OsStr, &mut [u8]) -> rustix::io::Result<usize>,
    budget: &mut MetadataCaptureBudget,
) -> io::Result<(Vec<Xattr>, Option<Vec<u8>>)> {
    let names = list_xattr_names(&mut list, budget)?;
    let mut xattr_count = 0usize;
    visit_xattr_names(&names, |name| {
        if name != POSIX_ACL_ACCESS {
            xattr_count = xattr_count.checked_add(1).ok_or_else(metadata_too_large)?;
        }
        Ok(())
    })?;

    let xattr_storage = xattr_count
        .checked_mul(size_of::<Xattr>())
        .ok_or_else(metadata_too_large)?;
    budget.check(xattr_storage)?;
    let mut xattrs = Vec::new();
    xattrs.try_reserve_exact(xattr_count).map_err(allocation_error)?;
    budget.record(xattrs.capacity().checked_mul(size_of::<Xattr>()).ok_or_else(metadata_too_large)?)?;

    let mut acl = None;
    visit_xattr_names(&names, |raw_name| {
        let name = OsStr::from_bytes(raw_name);
        let value = read_xattr_value(|buffer| get(name, buffer), budget)?;
        if raw_name == POSIX_ACL_ACCESS {
            acl = Some(value);
        } else {
            let name = copy_xattr_name(raw_name, budget)?;
            xattrs.push(Xattr { name, value });
        }
        Ok(())
    })?;
    Ok((xattrs, acl))
}

fn list_xattr_names(
    mut list: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    budget: &mut MetadataCaptureBudget,
) -> io::Result<Vec<u8>> {
    for _ in 0..XATTR_CAPTURE_ATTEMPTS {
        let capacity = match list(&mut []) {
            Ok(capacity) => capacity,
            Err(Errno::NOTSUP) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if capacity == 0 {
            return Ok(Vec::new());
        }
        budget.check(capacity)?;
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(capacity).map_err(allocation_error)?;
        buffer.resize(capacity, 0);
        match list(&mut buffer) {
            Ok(length) if length <= capacity => {
                buffer.truncate(length);
                budget.record(buffer.capacity())?;
                return Ok(buffer);
            }
            Ok(_) | Err(Errno::RANGE) => {}
            Err(Errno::NOTSUP) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "xattr names changed during repeated capture attempts",
    ))
}

fn read_xattr_value(
    mut get: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    budget: &mut MetadataCaptureBudget,
) -> io::Result<Vec<u8>> {
    for _ in 0..XATTR_CAPTURE_ATTEMPTS {
        let capacity = get(&mut [])?;
        if capacity == 0 {
            return Ok(Vec::new());
        }
        budget.check(capacity)?;
        let mut value = Vec::new();
        value.try_reserve_exact(capacity).map_err(allocation_error)?;
        value.resize(capacity, 0);
        match get(&mut value) {
            Ok(length) if length <= capacity => {
                value.truncate(length);
                budget.record(value.capacity())?;
                return Ok(value);
            }
            Ok(_) | Err(Errno::RANGE) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "xattr value changed during repeated capture attempts",
    ))
}

fn copy_xattr_name(name: &[u8], budget: &mut MetadataCaptureBudget) -> io::Result<OsString> {
    budget.check(name.len())?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(name.len()).map_err(allocation_error)?;
    owned.extend_from_slice(name);
    budget.record(owned.capacity())?;
    Ok(OsString::from_vec(owned))
}

fn visit_xattr_names(
    names: &[u8],
    mut visit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut start = 0;
    while start < names.len() {
        let end = names[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|relative| start + relative)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "filesystem returned malformed xattr names"))?;
        if end == start {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "filesystem returned an empty xattr name"));
        }
        visit(&names[start..end])?;
        start = end + 1;
    }
    Ok(())
}

pub(crate) fn apply_metadata(
    target: MetadataTarget<'_>,
    observation: &Observation,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
) -> io::Result<()> {
    match target {
        MetadataTarget::Fd(fd) => apply_fd_metadata(fd, observation, metadata, preservation),
        MetadataTarget::Symlink { parent, name } => {
            apply_symlink_metadata(parent, name, observation, metadata, preservation)
        }
    }
}

pub(crate) fn apply_final_metadata(
    target: MetadataTarget<'_>,
    metadata: &ExtendedMetadata,
) -> io::Result<()> {
    let flags = metadata.flags & BLOCKING_FLAGS;
    if flags == 0 {
        return Ok(());
    }
    match target {
        MetadataTarget::Fd(fd) => set_inode_flags(fd, metadata.flags),
        MetadataTarget::Symlink { .. } => Err(unsupported_metadata(
            "Linux cannot apply inode flags to symlinks",
        )),
    }
}

fn apply_fd_metadata(
    fd: BorrowedFd<'_>,
    observation: &Observation,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
) -> io::Result<()> {
    let destination = roots::observe_fd(fd)?;
    let copy_owner_matches = destination.uid == observation.uid && destination.gid == observation.gid;
    if preservation == Preservation::Move && !copy_owner_matches {
        fs::fchown(
            fd,
            Some(rustix::fs::Uid::from_raw(observation.uid)),
            Some(rustix::fs::Gid::from_raw(observation.gid)),
        )?;
    }

    remove_unpreserved_fd_xattrs(fd, metadata, preservation, copy_owner_matches)?;

    for xattr in &metadata.xattrs {
        if preservation == Preservation::Copy
            && !copy_owner_matches
            && xattr.name.as_bytes() == SECURITY_CAPABILITY
        {
            continue;
        }
        fs::fsetxattr(fd, &xattr.name, &xattr.value, fs::XattrFlags::empty())?;
    }
    if let Some(acl) = &metadata.acl {
        fs::fsetxattr(fd, POSIX_ACL_ACCESS, acl, fs::XattrFlags::empty())?;
    }

    let mut mode = observation.mode & 0o7777;
    if preservation == Preservation::Copy && !copy_owner_matches {
        mode &= !0o6000;
    }
    fs::fchmod(fd, fs::Mode::from_raw_mode(mode as _))?;
    apply_timestamps_fd(fd, observation.atime, observation.mtime)?;
    apply_prepublication_flags(fd, metadata.flags)
}

fn apply_symlink_metadata(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    observation: &Observation,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
) -> io::Result<()> {
    let before = roots::observe(parent, name)?;
    if metadata.flags != 0 {
        return Err(unsupported_metadata(
            "Linux cannot preserve inode flags on symlinks",
        ));
    }
    if preservation == Preservation::Move && (before.uid != observation.uid || before.gid != observation.gid) {
        fs::chownat(
            parent,
            name,
            Some(rustix::fs::Uid::from_raw(observation.uid)),
            Some(rustix::fs::Gid::from_raw(observation.gid)),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
    }
    if metadata.acl.is_some() {
        return Err(unsupported_metadata(
            "Linux does not support POSIX ACLs on symlinks",
        ));
    }
    let copy_owner_matches = before.uid == observation.uid && before.gid == observation.gid;
    let path = proc_fd_child(parent, name)?;
    remove_unpreserved_path_xattrs(&path, metadata, preservation, copy_owner_matches)?;
    for xattr in &metadata.xattrs {
        if preservation == Preservation::Copy
            && !copy_owner_matches
            && xattr.name.as_bytes() == SECURITY_CAPABILITY
        {
            continue;
        }
        fs::lsetxattr(&path, &xattr.name, &xattr.value, fs::XattrFlags::empty())?;
        ensure_link_identity(before, roots::observe(parent, name)?)?;
    }
    let times = Timestamps {
        last_access: timespec(observation.atime),
        last_modification: timespec(observation.mtime),
    };
    fs::utimensat(parent, name, &times, AtFlags::SYMLINK_NOFOLLOW)?;
    ensure_link_identity(before, roots::observe(parent, name)?)
}

fn apply_timestamps_fd(
    fd: BorrowedFd<'_>,
    atime: Timestamp,
    mtime: Timestamp,
) -> io::Result<()> {
    fs::futimens(
        fd,
        &Timestamps {
            last_access: timespec(atime),
            last_modification: timespec(mtime),
        },
    )?;
    Ok(())
}

fn timespec(timestamp: Timestamp) -> Timespec {
    Timespec {
        tv_sec: timestamp.seconds,
        tv_nsec: timestamp.nanoseconds.into(),
    }
}

fn remove_fd_xattr(fd: BorrowedFd<'_>, name: &[u8]) -> io::Result<()> {
    match fs::fremovexattr(fd, name) {
        Ok(()) | Err(Errno::NODATA) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_unpreserved_fd_xattrs(
    fd: BorrowedFd<'_>,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
    owner_matches: bool,
) -> io::Result<()> {
    let mut budget = MetadataCaptureBudget::new(METADATA_CAPTURE_BYTES);
    let names = list_xattr_names(|buffer| fs::flistxattr(fd, buffer), &mut budget)?;
    visit_xattr_names(&names, |name| {
        let preserved = metadata
            .xattrs
            .iter()
            .any(|xattr| xattr.name.as_bytes() == name)
            && !(preservation == Preservation::Copy
                && !owner_matches
                && name == SECURITY_CAPABILITY);
        if !preserved {
            remove_fd_xattr(fd, name)?;
        }
        Ok(())
    })
}

fn remove_unpreserved_path_xattrs(
    path: &OsStr,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
    owner_matches: bool,
) -> io::Result<()> {
    let mut budget = MetadataCaptureBudget::new(METADATA_CAPTURE_BYTES);
    let names = list_xattr_names(|buffer| fs::llistxattr(path, buffer), &mut budget)?;
    visit_xattr_names(&names, |name| {
        let preserved = metadata
            .xattrs
            .iter()
            .any(|xattr| xattr.name.as_bytes() == name)
            && !(preservation == Preservation::Copy
                && !owner_matches
                && name == SECURITY_CAPABILITY);
        if !preserved {
            match fs::lremovexattr(path, OsStr::from_bytes(name)) {
                Ok(()) | Err(Errno::NODATA) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })
}

fn apply_prepublication_flags(fd: BorrowedFd<'_>, source_flags: u32) -> io::Result<()> {
    validate_flag_set(source_flags)?;
    let flags = source_flags & !BLOCKING_FLAGS & !STRUCTURAL_FLAGS;
    set_inode_flags(fd, flags)
}

fn set_inode_flags(fd: BorrowedFd<'_>, source_flags: u32) -> io::Result<()> {
    validate_flag_set(source_flags)?;
    let current = match fs::ioctl_getflags(fd) {
        Ok(flags) => flags.bits(),
        Err(error) if flags_unsupported(error) => {
            if source_flags & !STRUCTURAL_FLAGS == 0 {
                return Ok(());
            }
            return Err(unsupported_metadata(
                "destination filesystem does not support inode flags",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let flags = (current & STRUCTURAL_FLAGS) | (source_flags & !STRUCTURAL_FLAGS);
    match fs::ioctl_setflags(fd, IFlags::from_bits_retain(flags)) {
        Ok(()) => Ok(()),
        Err(error) if flags_unsupported(error) => Err(unsupported_metadata(
            "destination filesystem cannot represent source inode flags",
        )),
        Err(error) => Err(error.into()),
    }
}

fn validate_flag_set(flags: u32) -> io::Result<()> {
    if flags & !(KNOWN_MUTABLE_FLAGS | STRUCTURAL_FLAGS) != 0 {
        return Err(unsupported_metadata(
            "source has inode flags that Linux cannot preserve",
        ));
    }
    Ok(())
}

fn capture_flags(fd: BorrowedFd<'_>) -> io::Result<u32> {
    match fs::ioctl_getflags(fd) {
        Ok(flags) => Ok(flags.bits()),
        Err(error) if flags_unsupported(error) => Ok(0),
        Err(error) => Err(error.into()),
    }
}


fn proc_fd_child(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<OsString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes.contains(&0) || bytes == b"." || bytes == b".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symlink metadata name is not one directory entry",
        ));
    }
    let mut path = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
    path.extend_from_slice(bytes);
    Ok(OsString::from_vec(path))
}

fn ensure_stable_link_metadata(before: Observation, after: Observation) -> io::Result<()> {
    if before.identity != after.identity
        || before.mode != after.mode
        || before.uid != after.uid
        || before.gid != after.gid
        || before.size != after.size
        || before.mtime != after.mtime
        || before.ctime != after.ctime
    {
        return Err(io::Error::other(
            "symlink changed while preserving metadata",
        ));
    }
    Ok(())
}

fn ensure_link_identity(before: Observation, after: Observation) -> io::Result<()> {
    if before.identity != after.identity {
        return Err(io::Error::other(
            "symlink identity changed while applying metadata",
        ));
    }
    Ok(())
}

fn check_cancelled(cancel: &Cancellation) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled"))
    } else {
        Ok(())
    }
}

fn reflink_unsupported(error: Errno) -> bool {
    matches!(error, Errno::XDEV | Errno::NOSYS | Errno::NOTSUP | Errno::NOTTY | Errno::INVAL)
}

fn cfr_unsupported(error: Errno) -> bool {
    matches!(error, Errno::XDEV | Errno::NOSYS | Errno::NOTSUP | Errno::INVAL)
}

fn sparse_seek_unsupported(error: Errno) -> bool {
    matches!(error, Errno::NOSYS | Errno::NOTSUP | Errno::INVAL)
}

fn flags_unsupported(error: Errno) -> bool {
    matches!(error, Errno::NOSYS | Errno::NOTSUP | Errno::NOTTY | Errno::INVAL)
}

fn unsupported_metadata(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod xattr_memory_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn short_value_charges_its_retained_capacity() {
        let mut budget = MetadataCaptureBudget::new(1024);
        let value = read_xattr_value(
            |buffer| {
                if buffer.is_empty() {
                    Ok(12)
                } else {
                    buffer[0] = 7;
                    Ok(1)
                }
            },
            &mut budget,
        )
        .unwrap();

        assert_eq!(value, [7]);
        assert_eq!(budget.used, value.capacity() + XATTR_ALLOCATION_OVERHEAD);
        assert!(value.capacity() < 4096);
    }

    #[test]
    fn growing_value_retries_and_charges_only_the_successful_buffer() {
        let call = Cell::new(0);
        let mut budget = MetadataCaptureBudget::new(1024);
        let value = read_xattr_value(
            |buffer| match call.replace(call.get() + 1) {
                0 => Ok(2),
                1 => Err(Errno::RANGE),
                2 => Ok(5),
                3 => {
                    buffer.copy_from_slice(b"hello");
                    Ok(5)
                }
                _ => unreachable!(),
            },
            &mut budget,
        )
        .unwrap();

        assert_eq!(value, b"hello");
        assert_eq!(call.get(), 4);
        assert_eq!(budget.used, value.capacity() + XATTR_ALLOCATION_OVERHEAD);
    }

    #[test]
    fn growing_name_list_retries_without_retaining_failed_capacity() {
        let call = Cell::new(0);
        let mut budget = MetadataCaptureBudget::new(1024);
        let names = list_xattr_names(
            |buffer| match call.replace(call.get() + 1) {
                0 => Ok(2),
                1 => Err(Errno::RANGE),
                2 => Ok(4),
                3 => {
                    buffer.copy_from_slice(b"a\0b\0");
                    Ok(4)
                }
                _ => unreachable!(),
            },
            &mut budget,
        )
        .unwrap();

        assert_eq!(names, b"a\0b\0");
        assert_eq!(call.get(), 4);
        assert_eq!(budget.used, names.capacity() + XATTR_ALLOCATION_OVERHEAD);
    }

    #[test]
    fn one_byte_xattr_does_not_keep_a_large_startup_buffer() {
        let mut budget = MetadataCaptureBudget::new(METADATA_CAPTURE_BYTES);
        let (xattrs, acl) = read_xattrs_with_budget(
            |buffer| {
                if buffer.is_empty() {
                    Ok(7)
                } else {
                    buffer.copy_from_slice(b"user.x\0");
                    Ok(7)
                }
            },
            |name, buffer| {
                assert_eq!(name.as_bytes(), b"user.x");
                if buffer.is_empty() {
                    Ok(1)
                } else {
                    buffer[0] = 9;
                    Ok(1)
                }
            },
            &mut budget,
        )
        .unwrap();

        assert!(acl.is_none());
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0].value, [9]);
        assert!(xattrs[0].value.capacity() < 4096);
        assert!(budget.used <= METADATA_CAPTURE_BYTES);
    }

    #[test]
    fn capture_budget_includes_owned_storage_and_stops_before_name_copy() {
        let mut budget = MetadataCaptureBudget::new(
            7 + XATTR_ALLOCATION_OVERHEAD
                + size_of::<Xattr>()
                + XATTR_ALLOCATION_OVERHEAD
                + 1
                + XATTR_ALLOCATION_OVERHEAD
                + 6
                + XATTR_ALLOCATION_OVERHEAD
                - 1,
        );
        let value_fills = Cell::new(0);
        let result = read_xattrs_with_budget(
            |buffer| {
                if buffer.is_empty() {
                    Ok(7)
                } else {
                    buffer.copy_from_slice(b"user.x\0");
                    Ok(7)
                }
            },
            |_, buffer| {
                if buffer.is_empty() {
                    Ok(1)
                } else {
                    value_fills.set(value_fills.get() + 1);
                    buffer[0] = 9;
                    Ok(1)
                }
            },
            &mut budget,
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(value_fills.get(), 1);
        assert!(budget.used <= budget.limit);
        assert!(budget.used > 7 + 6 + 1);
    }

    #[test]
    fn oversized_value_fails_after_size_query_and_before_allocation() {
        let mut budget = MetadataCaptureBudget::new(400);
        let value_fills = Cell::new(0);
        let result = read_xattrs_with_budget(
            |buffer| {
                if buffer.is_empty() {
                    Ok(7)
                } else {
                    buffer.copy_from_slice(b"user.x\0");
                    Ok(7)
                }
            },
            |_, buffer| {
                if buffer.is_empty() {
                    Ok(1024)
                } else {
                    value_fills.set(value_fills.get() + 1);
                    Ok(1024)
                }
            },
            &mut budget,
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(value_fills.get(), 0);
        assert!(budget.used <= budget.limit);
    }
}

#[cfg(test)]
mod staging_directory_tests {
    use super::*;
    use std::fs::{self as stdfs, File};
    use std::os::fd::AsFd;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let work = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("crate worktree ancestor")
                .join(".work/tmp");
            stdfs::create_dir_all(&work).expect("create shared test scratch");
            loop {
                let index = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = work.join(format!(
                    "native-linux-foreign-stage-{}-{index}",
                    std::process::id()
                ));
                match stdfs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create native test directory: {error}"),
                }
            }
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).expect("remove owned native test directory");
        }
    }

    #[test]
    fn foreign_owned_created_directory_can_be_prepared_by_its_held_identity() {
        let effective_uid = rustix::process::geteuid().as_raw();
        if effective_uid != 0 {
            eprintln!(
                "skip: euid {effective_uid} lacks the root CAP_CHOWN fixture required for foreign-owner staging preparation"
            );
            return;
        }

        let directory = TestDirectory::new();
        let parent = File::open(directory.path()).expect("open staging parent");
        let name = OsStr::new("stage");
        fs::mkdirat(parent.as_fd(), name, fs::Mode::from_raw_mode(0o700))
            .expect("create staging directory");
        let expected = roots::observe(parent.as_fd(), name)
            .expect("observe created staging directory")
            .identity;
        let foreign_uid = rustix::fs::Uid::from_raw(65_534);
        fs::chownat(
            parent.as_fd(),
            name,
            Some(foreign_uid),
            None,
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .expect("assign foreign fixture owner");
        assert_eq!(
            roots::observe(parent.as_fd(), name)
                .expect("observe foreign-owned staging directory")
                .uid,
            foreign_uid.as_raw()
        );

        let prepared = prepare_directory(parent.as_fd(), name, expected)
            .expect("prepare directory with matching identity after ownership change");
        let observed = roots::observe_fd(prepared.as_fd()).expect("observe prepared directory");
        assert_eq!(observed.identity, expected);
        assert_eq!(observed.uid, foreign_uid.as_raw());
        assert_eq!(observed.mode & 0o7777, 0o700);
    }
}

pub(crate) use fdu_posix::identity::mount_identity as mount_id;
