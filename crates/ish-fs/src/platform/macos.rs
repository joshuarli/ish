use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{self, CopyfileFlags, Mode, OFlags, Timestamps};
use rustix::io::Errno;

use crate::Cancellation;
use crate::platform::metadata::{CaptureBudget, allocation_error, too_large};
use crate::platform::{
    CopyMethod, CopyStats, ExtendedMetadata, MetadataTarget, Preservation, Xattr,
};
use crate::roots::{EntryKind, Identity, Observation, Timestamp, observe_fd};

const XATTR_BUFFER_LIMIT: usize = super::METADATA_CAPTURE_BYTES;
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
pub(super) const BLOCKING_FLAGS: u32 = libc::UF_IMMUTABLE
    | libc::UF_APPEND
    | libc::SF_IMMUTABLE
    | libc::SF_APPEND;
const RESOURCE_FORK_NAME: &CStr = c"com.apple.ResourceFork";
const RESOURCE_FORK_POSITION_LIMIT: u64 = u32::MAX as u64 + 1;
const PRIVATE_DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);
const COPYFILE_DATA_FLAGS: CopyfileFlags = CopyfileFlags::from_bits_retain(
    libc::COPYFILE_DATA | libc::COPYFILE_DATA_SPARSE,
);

pub(crate) struct ClonedFile {
    pub descriptor: OwnedFd,
    pub identity: Identity,
}

unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, kind: libc::c_int) -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

pub(crate) fn try_clone_to_dir(
    src: BorrowedFd<'_>,
    parent: BorrowedFd<'_>,
    name: &OsStr,
    source_metadata: &ExtendedMetadata,
) -> io::Result<Option<ClonedFile>> {
    if !is_single_component(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "clone name is not one path component"));
    }
    if !clone_metadata_is_accessible(src, parent, source_metadata)? {
        return Ok(None);
    }
    match fs::fclonefileat(src, parent, name, fs::CloneFlags::NOOWNERCOPY) {
        Ok(()) => {}
        Err(error) if is_acceleration_unsupported(error) => return Ok(None),
        Err(error) => return Err(error.into()),
    }

    let identity = crate::roots::observe(parent, name).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("clone at staging name {name:?} could not be identified and was left untouched: {error}"),
        )
    })?.identity;
    let mut cleanup = CloneCleanup::new(parent, name, identity);
    let result = (|| {
        require_entry_identity(parent, name, identity)?;
        let metadata = open_metadata_handle(parent, name)?;
        require_descriptor_identity(metadata.as_fd(), identity)?;
        let flags = fs::fstat(metadata.as_fd())?.st_flags;
        if flags & BLOCKING_FLAGS != 0 {
            set_flags(metadata.as_fd(), flags & !BLOCKING_FLAGS)?;
        }
        apply_acl(metadata.as_fd(), None)?;
        let current = fs::fstat(metadata.as_fd())?;
        let mode = (current.st_mode as u32 & 0o1777) | 0o700;
        fs::fchmod(metadata.as_fd(), Mode::from_bits_truncate(mode as _))?;
        require_descriptor_identity(metadata.as_fd(), identity)?;
        require_entry_identity(parent, name, identity)?;
        let descriptor = fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        require_descriptor_identity(descriptor.as_fd(), identity)?;
        Ok(ClonedFile { descriptor, identity })
    })();
    match result {
        Ok(clone) => {
            cleanup.disarm();
            Ok(Some(clone))
        }
        Err(error) => Err(cleanup.remove_after(error)),
    }
}

fn clone_metadata_is_accessible(
    src: BorrowedFd<'_>,
    parent: BorrowedFd<'_>,
    source_metadata: &ExtendedMetadata,
) -> io::Result<bool> {
    if fs::fstat(src)?.st_mode as u32 & 0o400 == 0 || source_metadata.acl.is_some() {
        return Ok(false);
    }
    let mut budget = CaptureBudget::new(XATTR_BUFFER_LIMIT);
    Ok(capture_acl(parent, &mut budget)?.is_none())
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
    let observation = crate::roots::observe(parent, name)?;
    if observation.identity != expected {
        return Err(io::Error::other("staging directory identity changed"));
    }
    if observation.mode & 0o500 != 0o500 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS filesystem staging requires owner read and search permission; a restrictive umask removing either is unsupported",
        ));
    }
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging directory name contains NUL"))?;
    // SAFETY: `parent` remains live and `name` is NUL-terminated for openat.
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
    copy_data_with_native_copy(src, dst, length, scratch, cancel, None, |progress_hook| {
        native_data_copy(src, dst, cancel, progress_hook)
    })
}

pub(crate) fn copy_data_with_native_copy(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
    progress_hook: Option<&(dyn Fn() + Send + Sync)>,
    native_copy: impl FnOnce(Option<&(dyn Fn() + Send + Sync)>) -> io::Result<u64>,
) -> io::Result<CopyStats> {
    if scratch.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "copy buffer is empty"));
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }

    match native_copy(progress_hook) {
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
        Err(_error) if cancel.is_cancelled() => Err(cancelled()),
        Err(error) if is_acceleration_unsupported_io(&error) => {
            fs::ftruncate(dst, 0)?;
            fs::ftruncate(dst, length)?;
            let streamed = super::buffered_copy(src, dst, length, scratch, cancel)?;
            Ok(CopyStats {
                method: CopyMethod::Buffered,
                logical_bytes: length,
                streamed_bytes: streamed,
            })
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn copy_resource_fork(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<()> {
    copy_resource_fork_inner(src, dst, scratch, cancel, None)
}

pub(crate) fn copy_resource_fork_inner(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    scratch: &mut [u8],
    cancel: &Cancellation,
    progress_hook: Option<&(dyn Fn() + Send + Sync)>,
) -> io::Result<()> {
    if scratch.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "copy buffer is empty"));
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let expected_size = capture_resource_fork_size(src)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "source resource fork disappeared before copy")
    })?;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    prepare_metadata_target(dst)?;
    let source_identity = observe_fd(src)?.identity;
    let destination_identity = observe_fd(dst)?.identity;
    let uses_stream_descriptors = expected_size > RESOURCE_FORK_POSITION_LIMIT;
    let source_stream = if uses_stream_descriptors {
        Some(open_resource_fork(src, source_identity, libc::O_RDONLY)?)
    } else {
        None
    };
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    fs::fsetxattr(
        dst,
        RESOURCE_FORK_NAME,
        &[],
        fs::XattrFlags::empty(),
    )?;
    let destination_stream = if uses_stream_descriptors {
        Some(open_resource_fork(dst, destination_identity, libc::O_WRONLY)?)
    } else {
        None
    };
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let source_size = match &source_stream {
        Some(source) => {
            let size = fs::fstat(source.as_fd())?.st_size;
            u64::try_from(size).map_err(|_| invalid_data("source resource fork has a negative length"))?
        }
        None => capture_resource_fork_size(src)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "source resource fork disappeared before copy")
        })?,
    };
    if source_size != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source resource fork length changed before copy",
        ));
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    if let Some(destination) = &destination_stream {
        fs::ftruncate(destination.as_fd(), expected_size)?;
    }
    let mut offset = 0_u64;
    while offset < expected_size {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let wanted = usize::try_from((expected_size - offset).min(scratch.len() as u64))
            .expect("bounded by resource fork buffer");
        let read = loop {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let result = match &source_stream {
                Some(source) => rustix::io::pread(source.as_fd(), &mut scratch[..wanted], offset)
                    .map_err(io::Error::from),
                None => read_resource_fork_chunk(src, &mut scratch[..wanted], offset),
            };
            match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "resource fork ended before captured length",
            ));
        }
        if read > wanted {
            return Err(invalid_data("resource fork read exceeded the requested length"));
        }
        let mut written = 0;
        while written < read {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let remaining = &scratch[written..read];
            match &destination_stream {
                Some(destination) => match rustix::io::pwrite(
                    destination.as_fd(),
                    remaining,
                    offset + written as u64,
                ) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "resource fork write made no progress")),
                    Ok(count) if count <= remaining.len() => written += count,
                    Ok(_) => return Err(invalid_data("resource fork write exceeded the requested length")),
                    Err(error) if error == Errno::INTR => continue,
                    Err(error) => return Err(error.into()),
                },
                None => {
                    write_resource_fork_chunk(dst, remaining, offset + written as u64)?;
                    written = read;
                }
            }
        }
        offset += read as u64;
        if let Some(progress_hook) = progress_hook {
            progress_hook();
        }
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    drop(destination_stream);
    drop(source_stream);
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


pub(crate) fn native_data_copy(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    cancel: &Cancellation,
    progress_hook: Option<&(dyn Fn() + Send + Sync)>,
) -> io::Result<u64> {
    fcopyfile_with_cancellation(src, dst, COPYFILE_DATA_FLAGS, cancel, progress_hook)?
        .ok_or_else(|| invalid_data("native data copy did not report its byte count"))
}

fn fcopyfile_with_cancellation(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    flags: CopyfileFlags,
    cancel: &Cancellation,
    progress_hook: Option<&(dyn Fn() + Send + Sync)>,
) -> io::Result<Option<u64>> {
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let mut state = CopyfileState::allocate()?;
    let mut context = CopyfileCallbackContext {
        cancel,
        progress_hook,
        panicked: AtomicBool::new(false),
    };
    let result = (|| {
        state.set(
            libc::COPYFILE_STATE_STATUS_CB as u32,
            copyfile_cancel_callback as *const () as *const libc::c_void,
        )?;
        state.set(
            libc::COPYFILE_STATE_STATUS_CTX as u32,
            (&mut context as *mut CopyfileCallbackContext<'_>).cast(),
        )?;
        // SAFETY: both descriptors and the allocated state remain live; Apple invokes the registered callback before this synchronous call returns.
        let result = unsafe { libc::fcopyfile(src.as_raw_fd(), dst.as_raw_fd(), state.0, flags.bits()) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if context.panicked.load(Ordering::Acquire) {
                return Err(io::Error::other("copyfile progress callback panicked"));
            }
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            return Err(error);
        }
        if context.panicked.load(Ordering::Acquire) {
            return Err(io::Error::other("copyfile progress callback panicked"));
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if flags.contains(CopyfileFlags::DATA) {
            state.copied().map(Some)
        } else {
            Ok(None)
        }
    })();
    let release = state.release();
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(release)) => Err(release),
        (Err(primary), Err(release)) => Err(io::Error::new(
            primary.kind(),
            format!("{primary}; copyfile state release failed: {release}"),
        )),
    }
}

struct CopyfileState(libc::copyfile_state_t);

impl CopyfileState {
    fn allocate() -> io::Result<Self> {
        // SAFETY: copyfile_state_alloc returns either a new opaque state or null and does not retain caller pointers.
        let state = unsafe { libc::copyfile_state_alloc() };
        if state.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(state))
        }
    }

    fn set(&self, flag: u32, value: *const libc::c_void) -> io::Result<()> {
        // SAFETY: `self.0` is allocated and live; each value pointer has the representation required by its copyfile state flag.
        let result = unsafe { libc::copyfile_state_set(self.0, flag, value) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn copied(&self) -> io::Result<u64> {
        let mut copied: libc::off_t = 0;
        // SAFETY: `self.0` is live and COPYFILE_STATE_COPIED writes an off_t to the provided pointer.
        let result = unsafe {
            libc::copyfile_state_get(
                self.0,
                libc::COPYFILE_STATE_COPIED as u32,
                (&mut copied as *mut libc::off_t).cast(),
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        u64::try_from(copied).map_err(|_| invalid_data("copyfile returned a negative byte count"))
    }

    fn release(&mut self) -> io::Result<()> {
        if self.0.is_null() {
            return Ok(());
        }
        let state = std::mem::replace(&mut self.0, std::ptr::null_mut());
        // SAFETY: this state was allocated by copyfile_state_alloc and is released at most once.
        let result = unsafe { libc::copyfile_state_free(state) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for CopyfileState {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct CopyfileCallbackContext<'a> {
    cancel: &'a Cancellation,
    progress_hook: Option<&'a (dyn Fn() + Send + Sync)>,
    panicked: AtomicBool,
}

extern "C" fn copyfile_cancel_callback(
    _what: libc::c_int,
    stage: libc::c_int,
    _state: libc::copyfile_state_t,
    _source: *const libc::c_char,
    _destination: *const libc::c_char,
    raw_context: *mut libc::c_void,
) -> libc::c_int {
    if raw_context.is_null() {
        return libc::COPYFILE_QUIT;
    }
    // SAFETY: fcopyfile_with_cancellation stores this live stack context in the copyfile state and keeps it alive until fcopyfile returns.
    let context = unsafe { &*raw_context.cast::<CopyfileCallbackContext<'_>>() };
    match catch_unwind(AssertUnwindSafe(|| {
        if stage == libc::COPYFILE_ERR {
            return libc::COPYFILE_QUIT;
        }
        if stage == libc::COPYFILE_PROGRESS {
            if let Some(progress_hook) = context.progress_hook {
                progress_hook();
            }
            if context.cancel.is_cancelled() {
                return libc::COPYFILE_QUIT;
            }
        }
        libc::COPYFILE_CONTINUE
    })) {
        Ok(decision) => decision,
        Err(_) => {
            context.panicked.store(true, Ordering::Release);
            libc::COPYFILE_QUIT
        }
    }
}

fn open_metadata_handle(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "clone name contains NUL"))?;
    // SAFETY: `parent` remains live and `name` is NUL-terminated for openat.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_EVTONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a newly owned descriptor returned by openat.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn open_resource_fork(
    fd: BorrowedFd<'_>,
    expected_identity: Identity,
    flags: libc::c_int,
) -> io::Result<OwnedFd> {
    require_descriptor_identity(fd, expected_identity)?;
    let base_path = descriptor_path(fd)?;
    let mut fork_path = base_path.as_bytes().to_vec();
    fork_path.extend_from_slice(b"/..namedfork/rsrc");
    let fork_path = CString::new(fork_path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "resource fork path contains NUL"))?;
    // SAFETY: `fork_path` is NUL-terminated and remains live for the open call.
    let raw = unsafe { libc::open(fork_path.as_ptr(), flags | libc::O_CLOEXEC, 0o600) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a newly owned descriptor returned by open.
    let fork = unsafe { OwnedFd::from_raw_fd(raw) };
    if observe_fd(fork.as_fd())?.identity != expected_identity {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named resource fork does not expose the base file identity",
        ));
    }
    Ok(fork)
}

fn read_resource_fork_chunk(
    fd: BorrowedFd<'_>,
    buffer: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let position = u32::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::Unsupported, "resource fork exceeds positioned xattr range"))?;
    // SAFETY: `fd` is live, the static xattr name is NUL-terminated, and `buffer` is writable for its length.
    let read = unsafe {
        libc::fgetxattr(
            fd.as_raw_fd(),
            RESOURCE_FORK_NAME.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            position,
            0,
        )
    };
    if read == -1 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(read).map_err(|_| invalid_data("resource fork read returned a negative length"))
    }
}

fn write_resource_fork_chunk(fd: BorrowedFd<'_>, buffer: &[u8], offset: u64) -> io::Result<()> {
    let position = u32::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::Unsupported, "resource fork exceeds positioned xattr range"))?;
    // SAFETY: `fd` is live, the static xattr name is NUL-terminated, and `buffer` is readable for its length.
    if unsafe {
        libc::fsetxattr(
            fd.as_raw_fd(),
            RESOURCE_FORK_NAME.as_ptr(),
            buffer.as_ptr().cast(),
            buffer.len(),
            position,
            0,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn descriptor_path(fd: BorrowedFd<'_>) -> io::Result<CString> {
    let mut path = [0_i8; libc::MAXPATHLEN as usize];
    // SAFETY: `fd` is live and `path` provides the MAXPATHLEN buffer required by F_GETPATH.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if !path.contains(&0) {
        return Err(invalid_data("file descriptor path was not NUL-terminated"));
    }
    // SAFETY: successful F_GETPATH writes a NUL-terminated path into the provided buffer.
    let path = unsafe { CStr::from_ptr(path.as_ptr()) };
    if path.to_bytes().is_empty() || !path.to_bytes().starts_with(b"/") {
        return Err(invalid_data("file descriptor has no absolute path"));
    }
    CString::new(path.to_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file descriptor path contains NUL"))
}

fn require_descriptor_identity(fd: BorrowedFd<'_>, expected: Identity) -> io::Result<()> {
    if observe_fd(fd)?.identity == expected {
        Ok(())
    } else {
        Err(io::Error::other("cloned staging descriptor identity changed"))
    }
}

fn require_entry_identity(parent: BorrowedFd<'_>, name: &OsStr, expected: Identity) -> io::Result<()> {
    if crate::roots::observe(parent, name)?.identity == expected {
        Ok(())
    } else {
        Err(io::Error::other("cloned staging entry identity changed"))
    }
}

struct CloneCleanup<'a> {
    parent: BorrowedFd<'a>,
    name: &'a OsStr,
    identity: Identity,
    armed: bool,
}

impl<'a> CloneCleanup<'a> {
    fn new(parent: BorrowedFd<'a>, name: &'a OsStr, identity: Identity) -> Self {
        Self { parent, name, identity, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn remove_after(&mut self, primary: io::Error) -> io::Error {
        self.armed = false;
        match remove_clone(self.parent, self.name, self.identity) {
            Ok(()) => primary,
            Err(cleanup) => io::Error::new(
                primary.kind(),
                format!("{primary}; cleanup of clone at staging name {:?} failed: {cleanup}", self.name),
            ),
        }
    }
}

impl Drop for CloneCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_clone(self.parent, self.name, self.identity);
        }
    }
}

fn remove_clone(parent: BorrowedFd<'_>, name: &OsStr, identity: Identity) -> io::Result<()> {
    match crate::roots::observe(parent, name) {
        Ok(observation) if observation.identity == identity => {
            let metadata = open_metadata_handle(parent, name)?;
            require_descriptor_identity(metadata.as_fd(), identity)?;
            let flags = fs::fstat(metadata.as_fd())?.st_flags;
            if flags & BLOCKING_FLAGS != 0 {
                set_flags(metadata.as_fd(), flags & !BLOCKING_FLAGS)?;
            }
            require_descriptor_identity(metadata.as_fd(), identity)?;
            require_entry_identity(parent, name, identity)?;
            fs::unlinkat(parent, name, fs::AtFlags::empty()).map_err(Into::into)
        }
        Ok(_) => Err(io::Error::other("staging name was replaced; clone location cannot be verified and replacement was left untouched")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::other("clone is absent from its staging name; current location cannot be verified")),
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
        if name.to_bytes() != RESOURCE_FORK_NAME.to_bytes() {
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
        if name.to_bytes() != RESOURCE_FORK_NAME.to_bytes() {
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
        if name.to_bytes() != RESOURCE_FORK_NAME.to_bytes()
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
            // SAFETY: acl_init allocates an empty ACL structure with no external references.
            let acl = unsafe { acl_init(0) };
            if acl.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `acl` is initialized and `fd` remains live for acl_set_fd_np.
            let set_result = unsafe { acl_set_fd_np(fd.as_raw_fd(), acl, ACL_TYPE_EXTENDED) };
            let set_error = (set_result == -1).then(io::Error::last_os_error);
            // SAFETY: `acl` is a releasable pointer returned by acl_init.
            let free_result = unsafe { acl_free(acl) };
            if let Some(error) = set_error {
                return Err(error);
            }
            if free_result == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
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

fn is_acceleration_unsupported_io(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ENOTSUP
            || code == libc::EOPNOTSUPP
            || code == libc::ENOSYS
            || code == libc::EXDEV
    )
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

pub(crate) use fdu_posix::identity::mount_identity as mount_id;
