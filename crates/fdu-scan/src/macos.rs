use crate::fsutil::{self, retry};
use crate::indexed::{BulkMetadata, DirectoryFacts, EntryHint, IndexedScanLimits};
use fdu_core::EntryType;
use rustix::fs::{self, Dir, DirEntry, FileType};
use std::cell::RefCell;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::thread;

mod attributes;

const BULK_RECORD_BUFFER_BYTES: usize = 64 * 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 128;
const STORAGE_WAIT_WORKER_HEADROOM: usize = 118;
const BASE_WORKER_CORE_RESERVE: usize = 2;
const INDEXED_TASKS_PER_WORKER: usize = 8;

pub(super) type DirectoryBuffer = attributes::AlignedBuffer<BULK_RECORD_BUFFER_BYTES>;

pub(super) fn indexed_scan_limits() -> IndexedScanLimits {
    let cores = thread::available_parallelism().map(usize::from).unwrap_or(1);
    let workers = cores.saturating_add(STORAGE_WAIT_WORKER_HEADROOM).min(INDEXED_SCAN_WORKER_LIMIT);
    let base_workers = cores.saturating_sub(BASE_WORKER_CORE_RESERVE).max(1);
    IndexedScanLimits {
        workers,
        base_workers: base_workers.min(workers),
        queued_directories: workers.saturating_mul(INDEXED_TASKS_PER_WORKER),
        retained_directory_budget: usize::MAX,
    }
}


thread_local! {
    static INDEXED_DIRECTORY_BUFFER: RefCell<Box<DirectoryBuffer>> =
        RefCell::new(Box::new(attributes::AlignedBuffer::new()));
}

pub(super) fn with_indexed_directory_buffer<T>(
    use_buffer: impl FnOnce(&mut DirectoryBuffer) -> T,
) -> T {
    INDEXED_DIRECTORY_BUFFER.with(|buffer| use_buffer(buffer.borrow_mut().as_mut()))
}



#[derive(Clone, Copy)]
pub(super) struct ScanHints;

pub(super) fn scan_hints(_root: BorrowedFd<'_>) -> ScanHints {
    ScanHints
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MountIdentity {
    device: u64,
    filesystem_id: [i32; 2],
    mount_point: Vec<u8>,
}


pub(super) enum StreamEntry<'a> {
    Bulk { name: &'a CStr, metadata: Option<BulkMetadata> },
    Listed(DirEntry),
}

impl StreamEntry<'_> {
    pub(super) fn name(&self) -> &CStr {
        match self {
            Self::Bulk { name, .. } => name,
            Self::Listed(entry) => entry.file_name(),
        }
    }

    pub(super) fn hint(&self) -> EntryHint {
        match self {
            Self::Bulk { metadata: Some(metadata), .. } if metadata.entry_type == EntryType::Directory => {
                EntryHint::Directory
            }
            Self::Bulk { metadata: Some(_), .. } => EntryHint::NotDirectory,
            Self::Bulk { metadata: None, .. } => EntryHint::Unknown,
            Self::Listed(entry) => match entry.file_type() {
                FileType::Directory => EntryHint::Directory,
                FileType::Unknown => EntryHint::Unknown,
                _ => EntryHint::NotDirectory,
            },
        }
    }

    pub(super) fn bulk(&self) -> Option<BulkMetadata> {
        match self {
            Self::Bulk { metadata, .. } => *metadata,
            Self::Listed(_) => None,
        }
    }
}

pub(super) struct IndexedDirectoryStream<'buffer> {
    directory: BorrowedFd<'buffer>,
    buffer: &'buffer mut DirectoryBuffer,
    offset: usize,
    remaining: usize,
    exhausted: bool,
    returned_any: bool,
    fallback: Option<Dir>,
}

impl<'buffer> IndexedDirectoryStream<'buffer> {
    pub(super) fn new(
        directory: BorrowedFd<'buffer>,
        buffer: &'buffer mut DirectoryBuffer,
        _hints: ScanHints,
    ) -> Self {
        Self {
            directory,
            buffer,
            offset: 0,
            remaining: 0,
            exhausted: false,
            returned_any: false,
            fallback: None,
        }
    }

    pub(super) fn next_entry(&mut self) -> io::Result<Option<StreamEntry<'_>>> {
        loop {
            if let Some(fallback) = &mut self.fallback {
                return match fallback.read() {
                    Some(Ok(entry)) => Ok(Some(StreamEntry::Listed(entry))),
                    Some(Err(error)) => Err(error.into()),
                    None => Ok(None),
                };
            }
            if self.exhausted {
                return Ok(None);
            }
            if self.remaining == 0 && !self.refill()? {
                return Ok(None);
            }
            if self.fallback.is_some() {
                continue;
            }

            let start = self.offset;
            let length = attributes::read_record_length(&self.buffer.as_bytes()[start..])?;
            let end = start
                .checked_add(length)
                .filter(|end| length >= 24 && *end <= self.buffer.as_bytes().len())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "macOS directory record exceeds its buffer",
                    )
                })?;
            self.offset = end;
            self.remaining -= 1;
            self.returned_any = true;
            let entry = attributes::parse_record(&self.buffer.as_bytes()[start..end])?;
            return Ok(Some(StreamEntry::Bulk { name: entry.name, metadata: entry.metadata }));
        }
    }

    fn refill(&mut self) -> io::Result<bool> {
        loop {
            let mut requested = attributes::requested_attributes();
            // SAFETY: the descriptor stays open for the lifetime of the stream, the attrlist is
            
            let count = unsafe {
                libc::getattrlistbulk(
                    self.directory.as_raw_fd(),
                    (&mut requested as *mut libc::attrlist).cast(),
                    self.buffer.as_mut_bytes().as_mut_ptr().cast(),
                    self.buffer.as_bytes().len(),
                    u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                )
            };
            if count > 0 {
                self.offset = 0;
                self.remaining = usize::try_from(count).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid macOS directory record count")
                })?;
                return Ok(true);
            }
            if count == 0 {
                self.exhausted = true;
                return Ok(false);
            }

            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let fallback_error = matches!(
                error.raw_os_error(),
                Some(libc::EACCES | libc::EPERM | libc::ENOTSUP | libc::EOPNOTSUPP | libc::ENOSYS)
            ) || error.kind() == io::ErrorKind::Unsupported;
            if !self.returned_any && fallback_error {
                
                self.fallback = Some(Dir::read_from(self.directory)?);
                return Ok(true);
            }
            return Err(error);
        }
    }
}


pub(super) fn mount_identity(fd: BorrowedFd<'_>) -> io::Result<MountIdentity> {
    let device = fsutil::identity_of(fd)?.device;
    let filesystem = retry(|| fs::fstatfs(fd))?;
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();

    Ok(MountIdentity {
        device,
        filesystem_id: fsid_words(&filesystem),
        mount_point,
    })
}

fn same_mount_on_fd(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
    device: u64,
) -> io::Result<bool> {
    let filesystem = retry(|| fs::fstatfs(fd))?;
    let mount_point = filesystem
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8);
    Ok(device == root_mount.device
        && fsid_words(&filesystem) == root_mount.filesystem_id
        && mount_point.eq(root_mount.mount_point.iter().copied()))
}


pub(super) fn describe_directory(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
) -> io::Result<DirectoryFacts> {
    let (identity, link_count, modified_time) = fsutil::identity_link_count_and_modified_time(fd)?;
    Ok(DirectoryFacts {
        identity,
        modified_time,
        link_count,
        same_mount: same_mount_on_fd(fd, root_mount, identity.device)?,
    })
}

fn fsid_words(filesystem: &rustix::fs::StatFs) -> [i32; 2] {
    
    const _: () = assert!(std::mem::size_of::<[i32; 2]>() == 8);
    // SAFETY: fsid_t is two i32 words, so reading eight bytes from it stays in bounds.
    unsafe { std::mem::transmute_copy::<_, [i32; 2]>(&filesystem.f_fsid) }
}
