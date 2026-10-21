use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use rustix::fs::{self, CopyfileFlags, Mode, Timestamps};
use rustix::io::Errno;

use crate::Cancellation;
use crate::platform::metadata::{CaptureBudget, allocation_error, too_large};
use crate::platform::{
    AccelerationError, CopyMethod, CopyStats, ExtendedMetadata, MetadataTarget, Preservation, Xattr,
};
use crate::roots::{EntryKind, Identity, Observation, Timestamp, observe_fd};

const XATTR_BUFFER_LIMIT: usize = super::METADATA_CAPTURE_BYTES;
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
pub(super) const BLOCKING_FLAGS: u32 = libc::UF_IMMUTABLE
    | libc::UF_APPEND
    | libc::SF_IMMUTABLE
    | libc::SF_APPEND;
const RESOURCE_FORK_NAME: &[u8] = b"com.apple.ResourceFork";
const PRIVATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);

unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, kind: libc::c_int) -> libc::c_int;
    fn acl_delete_fd_np(fd: libc::c_int, kind: libc::c_int) -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

pub(crate) fn try_clone_to_dir(
    src: BorrowedFd<'_>,
    parent: BorrowedFd<'_>,
    name: &OsStr,
) -> Result<(), AccelerationError> {
    if !is_single_component(name) {
        return Err(AccelerationError::Io(Errno::INVAL));
    }
    let result = fs::fclonefileat(
        src,
        parent,
        name,
        fs::CloneFlags::NOOWNERCOPY,
    );
    result.map_err(|error| {
        if is_acceleration_unsupported(error) {
            AccelerationError::Unsupported
        } else {
            AccelerationError::Io(error)
        }
    })
}

pub(crate) fn prepare_directory(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: Identity,
) -> io::Result<OwnedFd> {
    if expected.kind != EntryKind::Directory || !is_single_component(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging directory identity or name is invalid",
        ));
    }
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging directory name contains NUL"))?;
    // SAFETY: `parent` is live and `name` is NUL-terminated; O_EVTONLY opens only the entry for metadata and event access.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a newly owned descriptor returned by openat.
    let event_fd = unsafe { OwnedFd::from_raw_fd(raw) };
    verify_directory_identity(event_fd.as_fd(), expected)?;
    apply_acl(event_fd.as_fd(), None)?;
    fs::fchmod(event_fd.as_fd(), PRIVATE_DIRECTORY_MODE)?;
    let directory = fs::openat(
        parent,
        name.as_c_str(),
        fs::OFlags::RDONLY | fs::OFlags::DIRECTORY | fs::OFlags::NOFOLLOW | fs::OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    verify_directory_identity(directory.as_fd(), expected)?;
    let stat = fs::fstat(directory.as_fd())?;
    if stat.st_mode as u32 & 0o777 != 0o700 {
        return Err(io::Error::other("staging directory did not become mode 0700"));
    }
    Ok(directory)
}

pub(crate) fn copy_data(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<CopyStats> {
    if scratch.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "copy buffer is empty"));
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }

    match native_data_copy(src, dst) {
        Ok(copied) if copied >= length => {
            fs::ftruncate(dst, length)?;
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            Ok(CopyStats {
                method: CopyMethod::Native,
                logical_bytes: length,
                streamed_bytes: copied.min(length),
            })
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source ended before captured length",
        )),
        Err(error) if is_acceleration_unsupported(error) => {
            fs::ftruncate(dst, 0)?;
            let streamed = super::buffered_copy(src, dst, length, scratch, cancel)?;
            Ok(CopyStats {
                method: CopyMethod::Buffered,
                logical_bytes: length,
                streamed_bytes: streamed,
            })
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn copy_resource_fork(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
) -> io::Result<()> {
    let expected_size = capture_resource_fork_size(src)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "source resource fork disappeared before copy")
    })?;
    prepare_metadata_target(dst)?;
    let state = fs::copyfile_state_alloc()?;
    let result = unsafe {
        fs::fcopyfile(src, dst, state, CopyfileFlags::XATTR)
    };
    let free_result = unsafe { fs::copyfile_state_free(state) };
    result?;
    free_result?;
    let actual_size = capture_resource_fork_size(dst)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "destination resource fork is missing after copy")
    })?;
    if actual_size != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resource fork length changed during copy",
        ));
    }
    Ok(())
}

pub(crate) fn capture_metadata(target: MetadataTarget<'_>) -> io::Result<ExtendedMetadata> {
    with_target_fd(target, |fd| {
        let stat = fs::fstat(fd)?;
        let mut budget = CaptureBudget::new(XATTR_BUFFER_LIMIT);
        let xattrs = capture_xattrs(fd, &mut budget)?;
        let is_symlink = is_symlink(stat.st_mode);
        let acl = if is_symlink {
            None
        } else {
            capture_acl(fd, &mut budget)?
        };
        let resource_fork_size = capture_resource_fork_size(fd)?;
        if is_symlink && resource_fork_size.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "resource fork on a symlink cannot be preserved",
            ));
        }
        Ok(ExtendedMetadata {
            xattrs,
            acl,
            flags: stat.st_flags,
            birthtime: Some(Timestamp {
                seconds: stat.st_birthtime as i64,
                nanoseconds: stat.st_birthtime_nsec as u32,
            }),
            resource_fork_size,
        })
    })
}

pub(crate) fn apply_metadata(
    target: MetadataTarget<'_>,
    observation: &Observation,
    metadata: &ExtendedMetadata,
    preservation: Preservation,
) -> io::Result<()> {
    with_target_fd(target, |fd| {
        let current = fs::fstat(fd)?;
        let is_symlink = is_symlink(current.st_mode);
        prepare_metadata_target(fd)?;

        if preservation == Preservation::Move
            && (current.st_uid != observation.uid || current.st_gid != observation.gid)
        {
            fs::fchown(
                fd,
                Some(rustix::fs::Uid::from_raw(observation.uid)),
                Some(rustix::fs::Gid::from_raw(observation.gid)),
            )?;
        }

        reconcile_xattrs(fd, &metadata.xattrs)?;

        let owner_changed = preservation == Preservation::Copy
            && (current.st_uid != observation.uid || current.st_gid != observation.gid);
        let mut mode = observation.mode & 0o7777;
        if owner_changed {
            mode &= !(libc::S_ISUID as u32 | libc::S_ISGID as u32);
        }
        if !is_symlink {
            fs::fchmod(fd, Mode::from_bits_truncate(mode as _))?;
            apply_acl(fd, metadata.acl.as_deref())?;
        }

        fs::futimens(
            fd,
            &Timestamps {
                last_access: timestamp_to_timespec(observation.atime)?,
                last_modification: timestamp_to_timespec(observation.mtime)?,
            },
        )?;
        if let Some(birthtime) = metadata.birthtime {
            set_birthtime(fd, birthtime)?;
        }

        set_flags(fd, metadata.flags & !BLOCKING_FLAGS)?;
        Ok(())
    })
}

pub(crate) fn apply_final_metadata(
    target: MetadataTarget<'_>,
    metadata: &ExtendedMetadata,
) -> io::Result<()> {
    with_target_fd(target, |fd| set_flags(fd, metadata.flags))
}

pub(crate) fn mount_id(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut statfs = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `fd` is live and `statfs` has the exact writable ABI layout.
    let result = unsafe { libc::fstatfs(fd.as_raw_fd(), statfs.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatfs initialized the structure on success.
    let filesystem = unsafe { statfs.assume_init() };
    // SAFETY: Darwin defines fsid_t as two i32 values in its public ABI.
    let parts = unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(filesystem.f_fsid) };
    Ok((u64::from(parts[0] as u32) << 32) | u64::from(parts[1] as u32))
}

fn native_data_copy(src: BorrowedFd<'_>, dst: BorrowedFd<'_>) -> Result<u64, Errno> {
    let state = fs::copyfile_state_alloc()?;
    let flags = CopyfileFlags::DATA | CopyfileFlags::from_bits_retain(libc::COPYFILE_DATA_SPARSE);
    let result = unsafe { fs::fcopyfile(src, dst, state, flags) }
        .and_then(|()| unsafe { fs::copyfile_state_get_copied(state) });
    let free_result = unsafe { fs::copyfile_state_free(state) };
    match result {
        Ok(copied) => {
            free_result?;
            Ok(copied)
        }
        Err(error) => Err(error),
    }
}

fn capture_xattrs(fd: BorrowedFd<'_>, budget: &mut CaptureBudget) -> io::Result<Vec<Xattr>> {
    let names = list_xattr_names(fd, budget)?;
    let mut xattr_count = 0usize;
    let mut start = 0;
    while start < names.len() {
        let end = xattr_name_end(&names, start)?;
        let name = CStr::from_bytes_with_nul(&names[start..=end])
            .map_err(|_| invalid_data("filesystem returned a malformed xattr name"))?;
        if name.to_bytes() != RESOURCE_FORK_NAME {
            xattr_count = xattr_count.checked_add(1).ok_or_else(too_large)?;
        }
        start = end + 1;
    }

    let requested_storage = xattr_count.checked_mul(std::mem::size_of::<Xattr>()).ok_or_else(too_large)?;
    budget.check(requested_storage)?;
    let mut attrs = Vec::new();
    attrs.try_reserve_exact(xattr_count).map_err(allocation_error)?;
    budget.record(attrs.capacity().checked_mul(std::mem::size_of::<Xattr>()).ok_or_else(too_large)?)?;

    let mut start = 0;
    while start < names.len() {
        let end = xattr_name_end(&names, start)?;
        let name = CStr::from_bytes_with_nul(&names[start..=end])
            .map_err(|_| invalid_data("filesystem returned a malformed xattr name"))?;
        if name.to_bytes() != RESOURCE_FORK_NAME {
            let value = capture_xattr_value(fd, name, budget)?;
            let name = copy_xattr_name(name.to_bytes(), budget)?;
            attrs.push(Xattr { name, value });
        }
        start = end + 1;
    }
    Ok(attrs)
}

fn xattr_name_end(names: &[u8], start: usize) -> io::Result<usize> {
    let relative_end = names[start..]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid_data("filesystem returned a malformed xattr name list"))?;
    let end = start + relative_end;
    if end == start {
        return Err(invalid_data("filesystem returned an empty xattr name"));
    }
    Ok(end)
}

fn capture_xattr_value(
    fd: BorrowedFd<'_>,
    name: &CStr,
    budget: &mut CaptureBudget,
) -> io::Result<Vec<u8>> {
    let mut size_query = [0_u8; 0];
    let expected = fs::fgetxattr(fd, name, &mut size_query[..])?;
    budget.check(expected)?;
    let mut value = Vec::new();
    value.try_reserve_exact(expected).map_err(allocation_error)?;
    budget.record(value.capacity())?;
    value.resize(expected, 0);
    let value_length = fs::fgetxattr(fd, name, value.as_mut_slice())?;
    if value_length != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "extended attribute changed during capture",
        ));
    }
    value.truncate(value_length);
    Ok(value)
}

fn copy_xattr_name(name: &[u8], budget: &mut CaptureBudget) -> io::Result<OsString> {
    budget.check(name.len())?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(name.len()).map_err(allocation_error)?;
    budget.record(owned.capacity())?;
    owned.extend_from_slice(name);
    Ok(OsString::from_vec(owned))
}

fn copy_bytes_to_cstring(bytes: &[u8], budget: &mut CaptureBudget) -> io::Result<CString> {
    if bytes.contains(&0) {
        return Err(invalid_data("metadata name contains NUL"));
    }
    let capacity = bytes.len().checked_add(1).ok_or_else(too_large)?;
    budget.check(capacity)?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(capacity).map_err(allocation_error)?;
    budget.record(owned.capacity())?;
    owned.extend_from_slice(bytes);
    owned.push(0);
    CString::from_vec_with_nul(owned).map_err(|_| invalid_data("metadata name contains NUL"))
}

fn capture_resource_fork_size(fd: BorrowedFd<'_>) -> io::Result<Option<u64>> {
    let name = c"com.apple.ResourceFork";
    let mut size_query = [0_u8; 0];
    match fs::fgetxattr(fd, name, &mut size_query[..]) {
        Ok(length) => Ok(Some(length as u64)),
        Err(Errno::NOATTR) | Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn capture_acl(fd: BorrowedFd<'_>, budget: &mut CaptureBudget) -> io::Result<Option<Vec<u8>>> {
    // SAFETY: `fd` remains live for the call and ACL_TYPE_EXTENDED is the Darwin ACL type.
    let acl = unsafe { acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if is_absent_acl(error.raw_os_error()) {
            return Ok(None);
        }
        return Err(error);
    }

    let mut length = 0;
    // SAFETY: `acl` is a live ACL returned by acl_get_fd_np and `length` is writable.
    let text = unsafe { acl_to_text(acl, &mut length) };
    let text_result = if text.is_null() {
        Err(io::Error::last_os_error())
    } else if length < 0 {
        Err(invalid_data("ACL has an invalid text length"))
    } else if length as usize > budget.limit.saturating_add(1) {
        Err(too_large())
    } else {
        // SAFETY: acl_to_text returned a NUL-terminated string and `text` remains live.
        let bytes = unsafe { CStr::from_ptr(text) }.to_bytes();
        (|| {
            budget.check(bytes.len())?;
            let mut owned = Vec::new();
            owned.try_reserve_exact(bytes.len()).map_err(allocation_error)?;
            budget.record(owned.capacity())?;
            owned.extend_from_slice(bytes);
            Ok(owned)
        })()
    };
    if !text.is_null() {
        // SAFETY: `text` is a releasable pointer returned by acl_to_text.
        let _ = unsafe { acl_free(text.cast()) };
    }
    // SAFETY: `acl` is a releasable pointer returned by acl_get_fd_np.
    let free_result = unsafe { acl_free(acl) };
    let text = text_result?;
    if free_result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(text))
}

fn reconcile_xattrs(fd: BorrowedFd<'_>, desired: &[Xattr]) -> io::Result<()> {
    let mut budget = CaptureBudget::new(XATTR_BUFFER_LIMIT);
    let names = list_xattr_names(fd, &mut budget)?;
    let mut start = 0;
    while start < names.len() {
        let relative_end = names[start..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid_data("filesystem returned a malformed xattr name list"))?;
        let end = start + relative_end;
        if end == start {
            return Err(invalid_data("filesystem returned an empty xattr name"));
        }
        let name = CStr::from_bytes_with_nul(&names[start..=end])
            .map_err(|_| invalid_data("filesystem returned a malformed xattr name"))?;
        if name.to_bytes() != RESOURCE_FORK_NAME
            && !desired.iter().any(|wanted| wanted.name.as_bytes() == name.to_bytes())
        {
            fs::fremovexattr(fd, name)?;
        }
        start = end + 1;
    }
    drop(names);
    for xattr in desired {
        let mut budget = CaptureBudget::new(XATTR_BUFFER_LIMIT);
        let name = copy_bytes_to_cstring(xattr.name.as_bytes(), &mut budget)?;
        fs::fsetxattr(fd, name.as_c_str(), &xattr.value, fs::XattrFlags::empty())?;
    }
    Ok(())
}

fn list_xattr_names(fd: BorrowedFd<'_>, budget: &mut CaptureBudget) -> io::Result<Vec<u8>> {
    let mut size_query = [0_u8; 0];
    let length = match fs::flistxattr(fd, &mut size_query[..]) {
        Ok(length) => length,
        Err(error) if is_no_xattrs(error) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    budget.check(length)?;
    let mut names = Vec::new();
    names.try_reserve_exact(length).map_err(allocation_error)?;
    budget.record(names.capacity())?;
    names.resize(length, 0);
    let actual_length = fs::flistxattr(fd, names.as_mut_slice())?;
    if actual_length != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "extended attribute list changed during capture",
        ));
    }
    Ok(names)
}

fn apply_acl(fd: BorrowedFd<'_>, desired: Option<&[u8]>) -> io::Result<()> {
    match desired {
        Some(text) => {
            let mut budget = CaptureBudget::new(XATTR_BUFFER_LIMIT);
            let text = copy_bytes_to_cstring(text, &mut budget)?;
            // SAFETY: `text` is NUL-terminated and remains live during acl_from_text.
            let acl = unsafe { acl_from_text(text.as_ptr()) };
            if acl.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `acl` is a live ACL and `fd` remains open for the call.
            let set_result = unsafe { acl_set_fd_np(fd.as_raw_fd(), acl, ACL_TYPE_EXTENDED) };
            // SAFETY: `acl` is a releasable pointer returned by acl_from_text.
            let free_result = unsafe { acl_free(acl) };
            if set_result == -1 {
                return Err(io::Error::last_os_error());
            }
            if free_result == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        None => {
            // SAFETY: `fd` remains live and ACL_TYPE_EXTENDED is the Darwin ACL type.
            let result = unsafe { acl_delete_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if is_absent_acl(error.raw_os_error()) || is_unsupported_errno(error.raw_os_error()) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn prepare_metadata_target(fd: BorrowedFd<'_>) -> io::Result<()> {
    let current = fs::fstat(fd)?;
    if current.st_flags & BLOCKING_FLAGS != 0 {
        set_flags(fd, current.st_flags & !BLOCKING_FLAGS)?;
    }
    if !is_symlink(current.st_mode) {
        let mode = (current.st_mode as u32 & 0o1777) | 0o700;
        fs::fchmod(fd, Mode::from_bits_truncate(mode as _))?;
        apply_acl(fd, None)?;
    }
    Ok(())
}

fn set_birthtime(fd: BorrowedFd<'_>, timestamp: Timestamp) -> io::Result<()> {
    if timestamp.nanoseconds >= 1_000_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp nanoseconds"));
    }
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut value = libc::timespec {
        tv_sec: timestamp.seconds,
        tv_nsec: timestamp.nanoseconds.into(),
    };
    // SAFETY: `fd` is live; the attribute list and timespec use Darwin ABI layouts and remain writable.
    let result = unsafe {
        libc::fsetattrlist(
            fd.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut value as *mut libc::timespec).cast(),
            std::mem::size_of::<libc::timespec>(),
            0,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_flags(fd: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor accepted by fchflags.
    let result = unsafe { libc::fchflags(fd.as_raw_fd(), flags) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn with_target_fd<T>(
    target: MetadataTarget<'_>,
    operation: impl FnOnce(BorrowedFd<'_>) -> io::Result<T>,
) -> io::Result<T> {
    match target {
        MetadataTarget::Fd(fd) => operation(fd),
        MetadataTarget::Symlink { parent, name } => {
            let name = CString::new(name.as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "symlink name contains NUL"))?;
            // SAFETY: `parent` remains live, `name` is NUL-terminated, and O_SYMLINK opens the link itself.
            let raw = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_SYMLINK | libc::O_CLOEXEC,
                )
            };
            if raw == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a newly owned descriptor returned by openat.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            if !is_symlink(fs::fstat(fd.as_fd())?.st_mode) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "metadata target is no longer a symlink"));
            }
            operation(fd.as_fd())
        }
    }
}

fn timestamp_to_timespec(timestamp: Timestamp) -> io::Result<rustix::fs::Timespec> {
    if timestamp.nanoseconds >= 1_000_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp nanoseconds"));
    }
    Ok(rustix::fs::Timespec {
        tv_sec: timestamp.seconds,
        tv_nsec: timestamp.nanoseconds.into(),
    })
}

fn is_single_component(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty() && !bytes.contains(&0) && !bytes.contains(&b'/') && bytes != b"." && bytes != b".."
}

fn verify_directory_identity(fd: BorrowedFd<'_>, expected: Identity) -> io::Result<()> {
    let observation = observe_fd(fd)?;
    if observation.identity != expected {
        return Err(io::Error::other("staging directory identity changed"));
    }
    Ok(())
}

fn is_symlink(mode: u16) -> bool {
    mode & libc::S_IFMT == libc::S_IFLNK
}

fn is_acceleration_unsupported(error: Errno) -> bool {
    error == Errno::NOTSUP || error == Errno::OPNOTSUPP || error == Errno::NOSYS || error == Errno::XDEV
}

fn is_no_xattrs(error: Errno) -> bool {
    is_unsupported_errno(Some(error.raw_os_error()))
}

fn is_unsupported_errno(error: Option<i32>) -> bool {
    matches!(error, Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP)
}

fn is_absent_acl(error: Option<i32>) -> bool {
    matches!(error, Some(code) if code == libc::ENOENT || code == libc::ENOATTR)
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled")
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
