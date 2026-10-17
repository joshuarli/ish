use crate::fsutil::{self, retry};
use crate::{ScanExclusions, ScanReport, TopLevelDirectory};
use rayon::prelude::*;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawDir, ResolveFlags, Statx, StatxFlags};
use rustix::io::Errno;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::ffi::{CStr, OsString};
use std::io;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ops::Deref;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) mod platform;


const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;

const MAX_CACHED_DIRECTORY_ENTRY_CAPACITY: usize = 4096;
const MAX_CACHED_DIRECTORY_ENTRY_VECTORS: usize = 8;


const SERIAL_FILE_LIMIT: usize = 128;
const PARALLEL_FILE_BATCH: usize = 2048;

const NAME_RESERVE_HINT: usize = 512;
const ENTRY_RESERVE_HINT: usize = 64;
const EXT4_SUPER_MAGIC: u64 = 0xef53;
pub(crate) const EXT4_HTREE_EOF_32BIT: u64 = 0x7fff_ffff;
pub(crate) const EXT4_HTREE_EOF_64BIT: u64 = i64::MAX as u64;

const MAX_RECURSIVE_DIRECTORY_DEPTH: usize = 64;

thread_local! {
    static DIRECTORY_BUFFER: RefCell<Vec<MaybeUninit<u8>>> = const { RefCell::new(Vec::new()) };
}



struct DirectoryBuffer {
    bytes: Vec<MaybeUninit<u8>>,
    _worker: PhantomData<Rc<()>>,
}

impl DirectoryBuffer {
    fn with<T>(inspect: impl FnOnce(&mut [MaybeUninit<u8>]) -> T) -> T {
        let mut bytes = DIRECTORY_BUFFER.with(|buffer| mem::take(&mut *buffer.borrow_mut()));
        if bytes.is_empty() {
            bytes.resize(DIRECTORY_BUFFER_BYTES, MaybeUninit::uninit());
        }
        let mut buffer = Self {
            bytes,
            _worker: PhantomData,
        };
        inspect(&mut buffer.bytes)
    }
}

impl Drop for DirectoryBuffer {
    fn drop(&mut self) {
        let bytes = mem::take(&mut self.bytes);
        let _ = DIRECTORY_BUFFER.try_with(|buffer| {
            let mut cached = buffer.borrow_mut();
            if cached.is_empty() {
                *cached = bytes;
            }
        });
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DirectoryEntry {
    name_offset: usize,
    name_length: u16,
    kind: EntryKind,
}

thread_local! {
    static DIRECTORY_ENTRY_POOL: RefCell<Vec<Vec<DirectoryEntry>>> = const { RefCell::new(Vec::new()) };
}


struct PooledDirectoryEntries {
    entries: DirectoryEntryStorage,
    _worker: PhantomData<Rc<()>>,
}


enum DirectoryEntryStorage {
    Inline {
        entries: [DirectoryEntry; 2],
        len: usize,
    },
    Heap(Vec<DirectoryEntry>),
}

impl PooledDirectoryEntries {
    fn take() -> Self {
        Self {
            entries: DirectoryEntryStorage::Inline {
                entries: [DirectoryEntry {
                    name_offset: 0,
                    name_length: 0,
                    kind: EntryKind::Unknown,
                }; 2],
                len: 0,
            },
            _worker: PhantomData,
        }
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        match &self.entries {
            DirectoryEntryStorage::Inline { entries, .. } => entries.len(),
            DirectoryEntryStorage::Heap(entries) => entries.capacity(),
        }
    }

    fn reserve(&mut self, additional: usize) {
        if let DirectoryEntryStorage::Inline { entries, len } = &self.entries {
            if *len + additional <= entries.len() {
                return;
            }
            let mut heap = DIRECTORY_ENTRY_POOL.with(|pool| {
                pool.borrow_mut().pop().unwrap_or_default()
            });
            heap.reserve(*len + additional);
            heap.extend_from_slice(&entries[..*len]);
            self.entries = DirectoryEntryStorage::Heap(heap);
        } else if let DirectoryEntryStorage::Heap(entries) = &mut self.entries {
            entries.reserve(additional);
        }
    }

    fn push(&mut self, entry: DirectoryEntry, capacity_hint: usize) {
        if let DirectoryEntryStorage::Inline { entries, len } = &mut self.entries {
            if *len < entries.len() {
                entries[*len] = entry;
                *len += 1;
                return;
            }
            let additional = capacity_hint.saturating_sub(*len).max(1);
            self.reserve(additional);
        }
        let DirectoryEntryStorage::Heap(entries) = &mut self.entries else {
            unreachable!("inline entries promote before exceeding their capacity");
        };
        entries.push(entry);
    }
}

impl Deref for PooledDirectoryEntries {
    type Target = [DirectoryEntry];

    fn deref(&self) -> &Self::Target {
        match &self.entries {
            DirectoryEntryStorage::Inline { entries, len } => &entries[..*len],
            DirectoryEntryStorage::Heap(entries) => entries,
        }
    }
}

impl Drop for PooledDirectoryEntries {
    fn drop(&mut self) {
        let DirectoryEntryStorage::Heap(entries) = &mut self.entries else {
            return;
        };
        if entries.capacity() > MAX_CACHED_DIRECTORY_ENTRY_CAPACITY {
            return;
        }
        entries.clear();
        let entries = mem::take(entries);
        let _ = DIRECTORY_ENTRY_POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < MAX_CACHED_DIRECTORY_ENTRY_VECTORS {
                pool.push(entries);
            }
        });
    }
}

struct DirectoryEntries {
    names: DirectoryNames,
    entries: PooledDirectoryEntries,
    
    disk_size: u64,
}

struct DirectoryFrame<'a> {
    directory: OwnedFd,
    names: DirectoryNames,
    entries: PooledDirectoryEntries,
    next_entry: usize,
    disk_size: u64,
    overflowed: bool,
    position: SummaryScanPosition<'a>,
}


#[derive(Clone, Copy)]
struct SummaryScanPosition<'a> {
    exclusions: &'a ScanExclusions,
    exclusion_node: Option<usize>,
    ext4_eof_cookie: bool,
    depth: usize,
}

impl<'a> SummaryScanPosition<'a> {
    fn root(exclusions: &'a ScanExclusions, ext4_eof_cookie: bool) -> Self {
        Self {
            exclusions,
            exclusion_node: exclusions.root_node(),
            ext4_eof_cookie,
            depth: 0,
        }
    }

    fn child(self, name: &[u8]) -> Self {
        let exclusion_node = self.exclusions.child(self.exclusion_node, name);
        Self {
            exclusions: self.exclusions,
            exclusion_node: exclusion_node.filter(|node| self.exclusions.has_children(*node)),
            ext4_eof_cookie: self.ext4_eof_cookie,
            depth: self.depth + 1,
        }
    }

    fn excludes_child(self, name: &[u8]) -> bool {
        self.exclusions
            .child(self.exclusion_node, name)
            .is_some_and(|node| self.exclusions.excludes(node))
    }
}

enum ScannedEntry {
    Item(u64),
    MountBoundary,
    Directory {
        directory: OwnedFd,
        ext4_eof_cookie: bool,
    },
}

enum OpenedChildDirectory {
    MountBoundary,
    Directory(OwnedFd, bool),
}

enum UnknownEntry {
    Directory,
    Item(u64),
}


enum DirectoryNames {
    Inline([u8; 16]),
    Heap(Vec<u8>),
}

impl DirectoryNames {
    fn new() -> Self {
        Self::Inline([0; 16])
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline(bytes) => &bytes[..inline_name_bytes_len(bytes)],
            Self::Heap(names) => names,
        }
    }

    fn append_name(&mut self, name: &[u8], reserve_hint: usize) -> usize {
        let additional = name
            .len()
            .checked_add(1)
            .expect("directory name length is representable");
        if let Self::Inline(bytes) = self {
            let current_len = inline_name_bytes_len(bytes);
            if current_len + additional <= bytes.len() {
                bytes[current_len..current_len + name.len()].copy_from_slice(name);
                bytes[current_len + name.len()] = 0;
                return current_len;
            }

            let inline = *bytes;
            let capacity = current_len
                .saturating_add(reserve_hint)
                .max(current_len.saturating_add(additional));
            let mut names = Vec::with_capacity(capacity);
            names.extend_from_slice(&inline[..current_len]);
            names.extend_from_slice(name);
            names.push(0);
            *self = Self::Heap(names);
            return current_len;
        }

        let Self::Heap(names) = self else {
            unreachable!("inline names transition to heap storage before overflow");
        };
        let start = names.len();
        names.extend_from_slice(name);
        names.push(0);
        start
    }
}

#[cfg(target_pointer_width = "64")]
const _: [(); 24] = [(); mem::size_of::<DirectoryNames>()];


fn inline_name_bytes_len(bytes: &[u8; 16]) -> usize {
    bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last_name_byte| (last_name_byte + 2).min(bytes.len()))
}

impl DirectoryEntry {
    fn as_c_str<'a>(&self, names: &'a [u8]) -> &'a CStr {
        let end = self.name_offset + usize::from(self.name_length) + 1;
        // SAFETY: directory enumeration appends one terminator after each name and records its byte length.
        unsafe { CStr::from_bytes_with_nul_unchecked(&names[self.name_offset..end]) }
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum EntryKind {
    Directory,
    Other,
    Unknown,
}

const _: [(); 16] = [(); mem::size_of::<DirectoryEntry>()];



fn reports(stat: &Statx, flags: StatxFlags) -> bool {
    StatxFlags::from_bits_retain(stat.stx_mask).contains(flags)
}

fn is_directory(stat: &Statx) -> bool {
    reports(stat, StatxFlags::TYPE) && FileType::from_raw_mode(u32::from(stat.stx_mode)) == FileType::Directory
}


pub(super) fn scan(path: &Path, apparent: bool, exclusions: &ScanExclusions) -> io::Result<ScanReport> {
    if apparent {
        scan_mode::<true>(path, exclusions)
    } else {
        scan_mode::<false>(path, exclusions)
    }
}

fn scan_mode<const APPARENT: bool>(path: &Path, exclusions: &ScanExclusions) -> io::Result<ScanReport> {
    let root = fsutil::open_directory(path)?;
    let root_fd = root.as_fd();
    let ext4_eof_cookie = is_ext4(root_fd);
    let root_position = SummaryScanPosition::root(exclusions, ext4_eof_cookie);
    let skipped_entries = AtomicUsize::new(0);
    let mount_boundaries = AtomicUsize::new(0);
    let DirectoryEntries { names, entries, .. } = read_directory::<APPARENT, true>(
        root_fd,
        root_position,
        &skipped_entries,
    )?;
    let name_bytes = names.as_slice();

    let mut directories = entries
        .par_iter()
        .copied()
        .filter_map(|entry| {
            match scan_top_level_entry::<APPARENT>(
                root_fd,
                name_bytes,
                entry,
                root_position,
                &skipped_entries,
                &mount_boundaries,
            ) {
                Ok(directory) => directory,
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    directories.sort_unstable_by_key(|directory| Reverse(directory.disk_size));

    Ok(ScanReport {
        directories,
        skipped_entries: skipped_entries.load(Ordering::Relaxed),
        mount_boundaries: mount_boundaries.load(Ordering::Relaxed),
        unsupported_aliases: 0,
    })
}

fn scan_top_level_entry<const APPARENT: bool>(
    parent: BorrowedFd<'_>,
    names: &[u8],
    entry: DirectoryEntry,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<Option<TopLevelDirectory>> {
    let child_position = position.child(entry.as_c_str(names).to_bytes());
    if matches!(entry.kind, EntryKind::Other) {
        return Ok(None);
    }

    if matches!(entry.kind, EntryKind::Unknown) {
        let is_directory = with_stat_at(
            parent,
            entry.as_c_str(names),
            StatxFlags::TYPE,
            |stat| {
                if !reports(stat, StatxFlags::TYPE) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "filesystem did not report an unknown entry type",
                    ));
                }
                Ok(is_directory(stat))
            },
        )?;
        if !is_directory {
            return Ok(None);
        }
    }

    let (disk_size, exclusion) = match open_child_directory(
        parent,
        entry.as_c_str(names),
        child_position.ext4_eof_cookie,
    )? {
        OpenedChildDirectory::MountBoundary => {
            mount_boundaries.fetch_add(1, Ordering::Relaxed);
            (0, Some(fdu_core::ExclusionReason::MountBoundary))
        }
        OpenedChildDirectory::Directory(child, child_ext4_eof_cookie) => {
            let disk_size = scan_directory_contents::<APPARENT>(
                child,
                SummaryScanPosition {
                    ext4_eof_cookie: child_ext4_eof_cookie,
                    ..child_position
                },
                skipped_entries,
                mount_boundaries,
            )?;
            (disk_size, None)
        }
    };
    Ok(Some(TopLevelDirectory {
        name: OsString::from_vec(entry.as_c_str(names).to_bytes().to_vec()),
        disk_size,
        exclusion,
    }))
}

fn scan_directory_contents<const APPARENT: bool>(
    directory: OwnedFd,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    if position.depth >= MAX_RECURSIVE_DIRECTORY_DEPTH {
        return scan_directory_iterative::<APPARENT>(
            directory,
            position,
            skipped_entries,
            mount_boundaries,
        );
    }

    let DirectoryEntries {
        names,
        entries,
        disk_size,
    } = read_directory::<APPARENT, false>(directory.as_fd(), position, skipped_entries)?;
    if entries.is_empty() {
        return Ok(disk_size);
    }
    let name_bytes = names.as_slice();
    let directory_fd = directory.as_fd();
    let child_size = entries
        .par_iter()
        .copied()
        .map(|entry| {
            match scan_entry_size::<APPARENT>(
                directory_fd,
                name_bytes,
                entry,
                position,
                skipped_entries,
                mount_boundaries,
            ) {
                Ok(size) => Ok(size),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                    Ok(0)
                }
            }
        })
        .try_reduce(
            || 0u64,
            |left, right| {
                left.checked_add(right).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory total exceeds the supported byte-count range",
                    )
                })
            },
        )?;
    disk_size.checked_add(child_size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory total exceeds the supported byte-count range",
        )
    })
}

impl<'a> DirectoryFrame<'a> {
    fn new<const APPARENT: bool>(
        directory: OwnedFd,
        position: SummaryScanPosition<'a>,
        skipped_entries: &AtomicUsize,
    ) -> io::Result<Self> {
        let DirectoryEntries {
            names,
            entries,
            disk_size,
        } = read_directory::<APPARENT, false>(directory.as_fd(), position, skipped_entries)?;
        Ok(Self {
            directory,
            names,
            entries,
            next_entry: 0,
            disk_size,
            overflowed: false,
            position,
        })
    }

    fn add_size(&mut self, size: u64) {
        if let Some(total) = self.disk_size.checked_add(size) {
            self.disk_size = total;
        } else {
            self.overflowed = true;
        }
    }

    fn finish(self) -> io::Result<u64> {
        if self.overflowed {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory total exceeds the supported byte-count range",
            ))
        } else {
            Ok(self.disk_size)
        }
    }
}

fn scan_directory_iterative<const APPARENT: bool>(
    directory: OwnedFd,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    let root = DirectoryFrame::new::<APPARENT>(directory, position, skipped_entries)?;
    let mut stack = vec![root];

    loop {
        let entry = {
            let frame = stack.last_mut().expect("root directory frame remains present");
            match frame.entries.get(frame.next_entry) {
                Some(entry) => {
                    frame.next_entry += 1;
                    Some(*entry)
                }
                None => None,
            }
        };

        if let Some(entry) = entry {
            let scanned = {
                let frame = stack.last().expect("root directory frame remains present");
                let child_position = frame.position.child(entry.as_c_str(frame.names.as_slice()).to_bytes());
                (
                    scan_entry_iterative::<APPARENT>(
                        frame.directory.as_fd(),
                        frame.names.as_slice(),
                        entry,
                        frame.position.ext4_eof_cookie,
                    ),
                    child_position,
                )
            };

            let (scanned, child_position) = scanned;
            match scanned {
                Ok(ScannedEntry::Item(size)) => stack
                    .last_mut()
                    .expect("root directory frame remains present")
                    .add_size(size),
                Ok(ScannedEntry::MountBoundary) => {
                    mount_boundaries.fetch_add(1, Ordering::Relaxed);
                }
                Ok(ScannedEntry::Directory {
                    directory,
                    ext4_eof_cookie: child_ext4_eof_cookie,
                }) => {
                    let child_position = SummaryScanPosition {
                        ext4_eof_cookie: child_ext4_eof_cookie,
                        ..child_position
                    };
                    match DirectoryFrame::new::<APPARENT>(directory, child_position, skipped_entries) {
                    Ok(frame) => stack.push(frame),
                    Err(_) => {
                        skipped_entries.fetch_add(1, Ordering::Relaxed);
                    }
                    }
                }
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                }
            }
            continue;
        }

        let finished = stack
            .pop()
            .expect("root directory frame remains present")
            .finish();
        if let Some(parent) = stack.last_mut() {
            match finished {
                Ok(size) => parent.add_size(size),
                Err(_) => {
                    skipped_entries.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            return finished;
        }
    }
}

fn scan_entry_size<const APPARENT: bool>(
    parent: BorrowedFd<'_>,
    names: &[u8],
    entry: DirectoryEntry,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    let child_position = position.child(entry.as_c_str(names).to_bytes());
    match entry.kind {
        EntryKind::Directory => scan_child_directory::<APPARENT>(
            parent,
            entry.as_c_str(names),
            child_position,
            skipped_entries,
            mount_boundaries,
        ),
        EntryKind::Other => with_stat_at(
            parent,
            entry.as_c_str(names),
            entry_size_mask::<APPARENT>(),
            file_size::<APPARENT>,
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent,
                entry.as_c_str(names),
                StatxFlags::TYPE | entry_size_mask::<APPARENT>(),
                |stat| {
                    if !reports(stat, StatxFlags::TYPE) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "filesystem did not report an unknown entry type",
                        ));
                    }
                    if is_directory(stat) {
                        Ok(UnknownEntry::Directory)
                    } else {
                        file_size::<APPARENT>(stat).map(UnknownEntry::Item)
                    }
                },
            )?;
            match scanned {
                UnknownEntry::Directory => scan_child_directory::<APPARENT>(
                    parent,
                    entry.as_c_str(names),
                    child_position,
                    skipped_entries,
                    mount_boundaries,
                ),
                UnknownEntry::Item(size) => Ok(size),
            }
        }
    }
}

fn scan_entry_iterative<const APPARENT: bool>(
    parent: BorrowedFd<'_>,
    names: &[u8],
    entry: DirectoryEntry,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    match entry.kind {
        EntryKind::Directory => open_child_entry(
            parent,
            entry.as_c_str(names),
            ext4_eof_cookie,
        ),
        EntryKind::Other => with_stat_at(
            parent,
            entry.as_c_str(names),
            entry_size_mask::<APPARENT>(),
            |stat| file_size::<APPARENT>(stat).map(ScannedEntry::Item),
        ),
        EntryKind::Unknown => {
            let scanned = with_stat_at(
                parent,
                entry.as_c_str(names),
                StatxFlags::TYPE | entry_size_mask::<APPARENT>(),
                |stat| {
                    if !reports(stat, StatxFlags::TYPE) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "filesystem did not report an unknown entry type",
                        ));
                    }
                    if is_directory(stat) {
                        Ok(UnknownEntry::Directory)
                    } else {
                        file_size::<APPARENT>(stat).map(UnknownEntry::Item)
                    }
                },
            )?;
            match scanned {
                UnknownEntry::Directory => open_child_entry(
                    parent,
                    entry.as_c_str(names),
                    ext4_eof_cookie,
                ),
                UnknownEntry::Item(size) => Ok(ScannedEntry::Item(size)),
            }
        }
    }
}

fn entry_size_mask<const APPARENT: bool>() -> StatxFlags {
    if APPARENT {
        StatxFlags::SIZE
    } else {
        StatxFlags::BLOCKS
    }
}

fn scan_child_directory<const APPARENT: bool>(
    parent: BorrowedFd<'_>,
    name: &CStr,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
    mount_boundaries: &AtomicUsize,
) -> io::Result<u64> {
    let (child, child_ext4_eof_cookie) = match open_child_directory(
        parent,
        name,
        position.ext4_eof_cookie,
    )? {
        OpenedChildDirectory::MountBoundary => {
            mount_boundaries.fetch_add(1, Ordering::Relaxed);
            return Ok(0);
        }
        OpenedChildDirectory::Directory(child, child_ext4_eof_cookie) => {
            (child, child_ext4_eof_cookie)
        }
    };
    scan_directory_contents::<APPARENT>(
        child,
        SummaryScanPosition {
            ext4_eof_cookie: child_ext4_eof_cookie,
            ..position
        },
        skipped_entries,
        mount_boundaries,
    )
}

fn open_child_entry(
    parent: BorrowedFd<'_>,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<ScannedEntry> {
    match open_child_directory(parent, name, ext4_eof_cookie)? {
        OpenedChildDirectory::MountBoundary => Ok(ScannedEntry::MountBoundary),
        OpenedChildDirectory::Directory(directory, ext4_eof_cookie) => {
            Ok(ScannedEntry::Directory {
                directory,
                ext4_eof_cookie,
            })
        }
    }
}



fn open_child_directory(
    parent_fd: BorrowedFd<'_>,
    name: &CStr,
    ext4_eof_cookie: bool,
) -> io::Result<OpenedChildDirectory> {
    match fsutil::with_descriptor_retry(|| retry(|| openat2_directory(parent_fd, name))) {
        Ok(child) => Ok(OpenedChildDirectory::Directory(child, ext4_eof_cookie)),
        Err(error) if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
            Ok(OpenedChildDirectory::MountBoundary)
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code) if code == Errno::NOSYS.raw_os_error() || code == Errno::PERM.raw_os_error()
            ) =>
        {
            let child = fsutil::open_directory_at(parent_fd, name)?;
            if mount_id_for_fd(parent_fd)? == mount_id_for_fd(child.as_fd())? {
                Ok(OpenedChildDirectory::Directory(child, ext4_eof_cookie))
            } else {
                Ok(OpenedChildDirectory::MountBoundary)
            }
        }
        Err(error) => Err(error),
    }
}

fn openat2_directory(parent_fd: BorrowedFd<'_>, name: &CStr) -> rustix::io::Result<OwnedFd> {
    fs::openat2(
        parent_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_XDEV,
    )
}

#[inline(always)]
fn file_size<const APPARENT: bool>(stat: &Statx) -> io::Result<u64> {
    let size_mask = entry_size_mask::<APPARENT>();
    if !reports(stat, size_mask) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem did not report the requested file size",
        ));
    }
    if APPARENT {
        Ok(stat.stx_size)
    } else {
        stat.stx_blocks.checked_mul(512).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file total exceeds the supported byte-count range",
            )
        })
    }
}

fn with_stat_at<T>(
    parent_fd: BorrowedFd<'_>,
    name: &CStr,
    mask: StatxFlags,
    inspect: impl FnOnce(&Statx) -> io::Result<T>,
) -> io::Result<T> {
    
    let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC;
    let stat = retry(|| fs::statx(parent_fd, name, flags, mask))?;
    inspect(&stat)
}

fn mount_id_for_fd(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let stat = retry(|| fs::statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID))?;
    if reports(&stat, StatxFlags::MNT_ID) {
        Ok(stat.stx_mnt_id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report the directory mount ID",
        ))
    }
}

pub(super) fn is_ext4(fd: BorrowedFd<'_>) -> bool {
    retry(|| fs::fstatfs(fd)).is_ok_and(|filesystem| filesystem.f_type as u64 == EXT4_SUPER_MAGIC)
}



fn read_directory<const APPARENT: bool, const DIRECTORIES_ONLY: bool>(
    fd: BorrowedFd<'_>,
    position: SummaryScanPosition<'_>,
    skipped_entries: &AtomicUsize,
) -> io::Result<DirectoryEntries> {
    DirectoryBuffer::with(|buffer| {
        let mut collected = DirectoryEntries {
            names: DirectoryNames::new(),
            entries: PooledDirectoryEntries::take(),
            disk_size: 0,
        };
        let DirectoryEntries { names, entries, disk_size } = &mut collected;
        let mut files = FileBatch::default();
        let mut sized_in_place = 0usize;
        let mut directory = RawDir::new(fd, buffer);

        while let Some(record) = directory.next() {
            let record = match record {
                Ok(record) => record,
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            };
            
            let last_entry = position.ext4_eof_cookie
                && matches!(record.next_entry_cookie(), EXT4_HTREE_EOF_32BIT | EXT4_HTREE_EOF_64BIT);
            let name = record.file_name();
            let name_bytes = name.to_bytes();
            let kind = match record.file_type() {
                FileType::Directory => EntryKind::Directory,
                FileType::Unknown => EntryKind::Unknown,
                _ => EntryKind::Other,
            };

            let excluded = position.excludes_child(name_bytes);
            if name_bytes != b"." && name_bytes != b".." && !excluded {
                if !DIRECTORIES_ONLY && matches!(kind, EntryKind::Other) {
                    if sized_in_place < SERIAL_FILE_LIMIT {
                        
                        sized_in_place += 1;
                        match with_stat_at(fd, name, entry_size_mask::<APPARENT>(), |stat| {
                            file_size::<APPARENT>(stat)
                        }) {
                            Ok(size) => add_to_total(disk_size, size)?,
                            Err(_) => {
                                skipped_entries.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        files.push(name_bytes);
                        if files.entries.len() >= PARALLEL_FILE_BATCH {
                            add_to_total(disk_size, files.size::<APPARENT>(fd, skipped_entries)?)?;
                        }
                    }
                } else if !DIRECTORIES_ONLY || !matches!(kind, EntryKind::Other) {
                    let length = u16::try_from(name_bytes.len()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "directory entry name exceeds the supported length",
                        )
                    })?;
                    let start = names.append_name(name_bytes, NAME_RESERVE_HINT);
                    entries.push(
                        DirectoryEntry {
                            name_offset: start,
                            name_length: length,
                            kind,
                        },
                        ENTRY_RESERVE_HINT,
                    );
                }
            }
            if last_entry {
                break;
            }
        }
        if !files.entries.is_empty() {
            add_to_total(disk_size, files.size::<APPARENT>(fd, skipped_entries)?)?;
        }
        Ok(collected)
    })
}

fn add_to_total(total: &mut u64, size: u64) -> io::Result<()> {
    *total = total.checked_add(size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory total exceeds the supported byte-count range",
        )
    })?;
    Ok(())
}


#[derive(Default)]
struct FileBatch {
    names: Vec<u8>,
    entries: Vec<DirectoryEntry>,
}

impl FileBatch {
    fn push(&mut self, name: &[u8]) {
        self.entries.push(DirectoryEntry {
            name_offset: self.names.len(),
            name_length: name.len() as u16,
            kind: EntryKind::Other,
        });
        self.names.extend_from_slice(name);
        self.names.push(0);
    }

    
    fn size<const APPARENT: bool>(
        &mut self,
        fd: BorrowedFd<'_>,
        skipped_entries: &AtomicUsize,
    ) -> io::Result<u64> {
        let total = self
            .entries
            .par_iter()
            .map(|entry| {
                match with_stat_at(fd, entry.as_c_str(&self.names), entry_size_mask::<APPARENT>(), |stat| {
                    file_size::<APPARENT>(stat)
                }) {
                    Ok(size) => Ok(size),
                    Err(_) => {
                        skipped_entries.fetch_add(1, Ordering::Relaxed);
                        Ok(0)
                    }
                }
            })
            .try_reduce(
                || 0u64,
                |left, right| {
                    left.checked_add(right).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "directory total exceeds the supported byte-count range",
                        )
                    })
                },
            );
        self.entries.clear();
        self.names.clear();
        total
    }
}

