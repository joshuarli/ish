







use super::reports;
use crate::fsutil::{self, retry};
use crate::indexed::{DirectoryFacts, EntryHint, IndexedScanLimits};
use fdu_core::{FileIdentity, ModifiedTime};
use rustix::fs::{self, AtFlags, FileType, RawDir, RawDirEntry, StatxFlags};
use std::ffi::CStr;
use rustix::io::Errno;
use std::cell::RefCell;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::BorrowedFd;
use std::thread;

const INDEXED_DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
const INDEXED_SCAN_WORKER_LIMIT: usize = 64;
const INDEXED_SCAN_WORKER_HEADROOM: usize = 24;
const BASE_WORKERS_PER_CORE: usize = 2;
const INDEXED_TASKS_PER_WORKER: usize = 8;
const RESERVED_DESCRIPTORS: usize = 128;
const DESCRIPTOR_BUDGET_DIVISOR: usize = 4;
const MAX_SOFT_DESCRIPTOR_LIMIT: u64 = 1 << 20;

pub(crate) type DirectoryBuffer = Vec<MaybeUninit<u8>>;

thread_local! {
    static INDEXED_DIRECTORY_BUFFER: RefCell<DirectoryBuffer> = const { RefCell::new(Vec::new()) };
}


pub(crate) fn with_indexed_directory_buffer<T>(
    use_buffer: impl FnOnce(&mut DirectoryBuffer) -> T,
) -> T {
    INDEXED_DIRECTORY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if buffer.len() < INDEXED_DIRECTORY_BUFFER_BYTES {
            buffer.resize(INDEXED_DIRECTORY_BUFFER_BYTES, MaybeUninit::uninit());
        }
        use_buffer(&mut buffer)
    })
}






pub(crate) fn indexed_scan_limits() -> IndexedScanLimits {
    let descriptors = fsutil::raise_descriptor_limit(MAX_SOFT_DESCRIPTOR_LIMIT);
    let cores = thread::available_parallelism().map(usize::from).unwrap_or(1);
    let workers = cores.saturating_add(INDEXED_SCAN_WORKER_HEADROOM).min(INDEXED_SCAN_WORKER_LIMIT);
    let base = cores.saturating_mul(BASE_WORKERS_PER_CORE);
    limits_within_budget(workers, base, descriptors, INDEXED_TASKS_PER_WORKER)
}

fn limits_within_budget(workers: usize, base: usize, descriptor_limit: usize, per_worker: usize) -> IndexedScanLimits {
    let usable_descriptors = descriptor_limit.saturating_sub(RESERVED_DESCRIPTORS);
    let queue_budget = usable_descriptors / DESCRIPTOR_BUDGET_DIVISOR;
    let workers = workers.min(queue_budget).max(1);
    IndexedScanLimits {
        workers,
        base_workers: base.clamp(1, workers),
        retained_directory_budget: usable_descriptors.saturating_sub(2 * (workers + 1)).max(1),
        queued_directories: workers
            .saturating_mul(per_worker)
            .min(queue_budget)
            .max(1),
    }
}


pub(crate) struct StreamEntry<'a>(RawDirEntry<'a>);

impl StreamEntry<'_> {
    pub(crate) fn name(&self) -> &CStr {
        self.0.file_name()
    }

    pub(crate) fn hint(&self) -> EntryHint {
        match self.0.file_type() {
            FileType::Directory => EntryHint::Directory,
            FileType::Unknown => EntryHint::Unknown,
            _ => EntryHint::NotDirectory,
        }
    }

    pub(crate) fn bulk(&self) -> Option<crate::indexed::BulkMetadata> {
        None
    }
}



#[derive(Clone, Copy)]
pub(crate) struct ScanHints {
    
    
    end_cookie: bool,
}

pub(crate) fn scan_hints(root: BorrowedFd<'_>) -> ScanHints {
    ScanHints { end_cookie: super::is_ext4(root) }
}

pub(crate) struct IndexedDirectoryStream<'a> {
    directory: RawDir<'a, BorrowedFd<'a>>,
    stop_at_end_cookie: bool,
    finished: bool,
}

impl<'a> IndexedDirectoryStream<'a> {
    pub(crate) fn new(directory: BorrowedFd<'a>, buffer: &'a mut DirectoryBuffer, hints: ScanHints) -> Self {
        Self {
            directory: RawDir::new(directory, buffer),
            stop_at_end_cookie: hints.end_cookie,
            finished: false,
        }
    }

    
    pub(crate) fn next_entry(&mut self) -> io::Result<Option<StreamEntry<'_>>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            match self.directory.next() {
                None => return Ok(None),
                Some(Err(Errno::INTR)) => continue,
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(entry)) => {
                    self.finished = self.stop_at_end_cookie
                        && matches!(entry.next_entry_cookie(), super::EXT4_HTREE_EOF_32BIT | super::EXT4_HTREE_EOF_64BIT);
                    return Ok(Some(StreamEntry(entry)));
                }
            }
        }
    }
}



pub(crate) use fdu_posix::identity::MountIdentity;

fn describe(fd: BorrowedFd<'_>) -> io::Result<(FileIdentity, ModifiedTime, u64, u64)> {
    let mask = StatxFlags::INO | StatxFlags::NLINK | StatxFlags::MNT_ID | StatxFlags::MTIME;
    let stat = retry(|| fs::statx(fd, c"", AtFlags::EMPTY_PATH, mask))?;
    if !reports(&stat, mask) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report the directory identity and mount ID",
        ));
    }
    let identity = FileIdentity {
        device: fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
        inode: stat.stx_ino,
    };
    let modified_time = ModifiedTime::from_unix_seconds(stat.stx_mtime.tv_sec).unwrap_or_default();
    Ok((identity, modified_time, u64::from(stat.stx_nlink), stat.stx_mnt_id))
}

pub(crate) use fdu_posix::identity::mount_identity;

pub(crate) fn describe_directory(
    fd: BorrowedFd<'_>,
    root_mount: &MountIdentity,
) -> io::Result<DirectoryFacts> {
    let (identity, modified_time, link_count, mount_id) = describe(fd)?;
    Ok(DirectoryFacts {
        identity,
        modified_time,
        link_count,
        same_mount: identity.device == root_mount.device && mount_id == root_mount.mount_id,
    })
}

