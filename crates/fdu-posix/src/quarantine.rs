use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use crate::identity::{self, DIRECTORY_FLAGS, EntryIdentity, EntryType, FileIdentity, MountIdentity, directory_state, retry};

const PREFIX: &[u8] = b".fdu-quarantine-v2-";
const LEGACY_PREFIX: &[u8] = b".fdu-quarantine-v1-";
const MAGIC: &[u8] = b"fdu quarantine\0\x02";
const LEGACY_MAGIC: &[u8] = b"fdu quarantine\0\x01";
const INTENT_MAGIC: &[u8] = b"fdu capture\0\x02";
const LEGACY_INTENT_MAGIC: &[u8] = b"fdu capture\0\x01";
const MAX_RECORD: usize = 1024 * 1024;

pub struct Session {
    owner: rustix::process::Pid,
    root: RootLock,
    directory: OwnedFd,
    name: OsString,
    mount: MountIdentity,
    next: AtomicU64,
    recovery_required: AtomicBool,
}

struct RootLock {
    directory: fs::Dir,
    owner: rustix::process::Pid,
}

impl AsFd for RootLock {
    fn as_fd(&self) -> BorrowedFd<'_> { self.directory.fd().expect("live root directory must have a descriptor") }
}

impl RootLock {
    fn release(&self) -> io::Result<()> {
        if rustix::process::getpid() == self.owner { retry(|| fs::flock(self.as_fd(), fs::FlockOperation::Unlock))?; }
        Ok(())
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        self.release().expect("live root must release its acquired lock");
    }
}

#[derive(Clone, Debug)]
pub struct Ancestor {
    pub name: OsString,
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RecordKind { Retirement, Stage, Replacement(Publication) }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication {
    source: EntryIdentity,
    destination_parent: FileIdentity,
    destination_name: OsString,
}

impl Publication {
    pub fn new(source: EntryIdentity, destination_parent: FileIdentity, destination_name: OsString) -> io::Result<Self> {
        validate_name(destination_name.as_bytes())?;
        kind_byte(source.kind)?;
        Ok(Self { source, destination_parent, destination_name })
    }
}

#[derive(Clone, Debug)]
struct Intent {
    root: FileIdentity,
    parents: Vec<Ancestor>,
    name: OsString,
    expected: Option<EntryIdentity>,
    kind: RecordKind,
}

pub struct Retirement {
    parents: Vec<Ancestor>,
    name: OsString,
    expected: EntryIdentity,
}

impl Retirement {
    pub fn new(parents: Vec<Ancestor>, name: OsString, expected: EntryIdentity) -> io::Result<Self> {
        validate_name(name.as_bytes())?;
        for parent in &parents { validate_name(parent.name.as_bytes())?; }
        if expected.kind == EntryType::Root { return Err(invalid("a recovery root cannot be retired")); }
        Ok(Self { parents, name, expected })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    IntentPublished,
    Published,
    BeforeCapture,
    Captured,
    CaptureParentSynced,
    CaptureDirectorySynced,
    BeforeUnlink,
    Unlinked,
    RemovalDirectorySynced,
    BeforeParentSync,
    ParentSynced,
    IntentRemoved,
    IntentRemovalSynced,
    Restored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Removal { Removed, AlreadyAbsent, Changed }

#[derive(Debug)]
pub struct RemovalError {
    pub removed: bool,
    pub error: io::Error,
}

impl std::fmt::Display for RemovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.error.fmt(f) }
}
impl std::error::Error for RemovalError {}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }
fn sync(fd: BorrowedFd<'_>) -> io::Result<()> { retry(|| fs::fsync(fd)) }

fn visit_names(fd: BorrowedFd<'_>, mut visit: impl FnMut(OsString) -> io::Result<()>) -> io::Result<()> {
    for entry in fs::Dir::read_from(fd)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." { visit(OsString::from_vec(name.to_vec()))?; }
    }
    Ok(())
}

fn read(fd: BorrowedFd<'_>, name: &OsStr, legacy: bool) -> io::Result<Vec<u8>> {
    let file = retry(|| fs::openat(fd, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty()))?;
    let metadata = retry(|| fs::fstat(&file))?;
    if identity::entry_type(&metadata) != EntryType::RegularFile || metadata.st_size < 0 || metadata.st_size as u64 > MAX_RECORD as u64 || metadata.st_uid != rustix::process::geteuid().as_raw() || metadata.st_nlink != 1 || (!legacy && metadata.st_mode & 0o077 != 0) {
        return Err(invalid("recovery record owner, mode, type, or size is invalid"));
    }
    let mut bytes = Vec::new();
    let mut file = std::fs::File::from(file);
    Read::by_ref(&mut file).take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    let after = retry(|| fs::fstat(&file))?;
    if identity::identity(&metadata)? != identity::identity(&after)? || metadata.st_size != after.st_size || metadata.st_mode != after.st_mode || metadata.st_uid != after.st_uid || metadata.st_gid != after.st_gid || metadata.st_nlink != after.st_nlink || metadata.st_mtime != after.st_mtime || metadata.st_mtime_nsec != after.st_mtime_nsec || metadata.st_ctime != after.st_ctime || metadata.st_ctime_nsec != after.st_ctime_nsec { return Err(invalid("recovery record changed while reading")); }
    if bytes.len() > MAX_RECORD { return Err(invalid("recovery record exceeds bound")); }
    Ok(bytes)
}

fn publish(fd: BorrowedFd<'_>, temporary: &OsStr, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_RECORD { return Err(invalid("recovery record exceeds bound")); }
    let record = retry(|| fs::openat(fd, temporary, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR))?;
    retry(|| fs::fchmod(&record, Mode::RUSR | Mode::WUSR))?;
    let mut file = std::fs::File::from(record);
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::renameat_with(fd, temporary, fd, name, fs::RenameFlags::NOREPLACE)?;
    sync(fd)
}

fn put_identity(bytes: &mut Vec<u8>, identity: FileIdentity) {
    bytes.extend_from_slice(&identity.device.to_le_bytes());
    bytes.extend_from_slice(&identity.inode.to_le_bytes());
}
#[cfg(target_os = "linux")]
fn put_mount(bytes: &mut Vec<u8>, mount: &MountIdentity) -> io::Result<()> {
    bytes.extend_from_slice(&mount.device.to_le_bytes());
    bytes.extend_from_slice(&mount.mount_id.to_le_bytes());
    Ok(())
}

#[cfg(target_os = "linux")]
fn take_mount(bytes: &mut &[u8]) -> io::Result<MountIdentity> {
    let device = u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap());
    let mount_id = u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap());
    Ok(MountIdentity { device, mount_id })
}

#[cfg(target_os = "macos")]
fn put_mount(bytes: &mut Vec<u8>, mount: &MountIdentity) -> io::Result<()> {
    bytes.extend_from_slice(&mount.device.to_le_bytes());
    for word in mount.fsid { bytes.extend_from_slice(&word.to_le_bytes()); }
    bytes.extend_from_slice(&u32::try_from(mount.mount_point.len()).map_err(|_| invalid("mount name exceeds bound"))?.to_le_bytes());
    bytes.extend_from_slice(&mount.mount_point);
    Ok(())
}

#[cfg(target_os = "macos")]
fn take_mount(bytes: &mut &[u8]) -> io::Result<MountIdentity> {
    let device = u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap());
    let first = i32::from_le_bytes(take(bytes, 4)?.try_into().unwrap());
    let second = i32::from_le_bytes(take(bytes, 4)?.try_into().unwrap());
    let length = u32::from_le_bytes(take(bytes, 4)?.try_into().unwrap()) as usize;
    let mount_point = take(bytes, length)?.to_vec();
    Ok(MountIdentity { device, fsid: [first, second], mount_point })
}

fn put_name(bytes: &mut Vec<u8>, name: &OsStr) -> io::Result<()> {
    validate_name(name.as_bytes())?;
    bytes.extend_from_slice(&u32::try_from(name.len()).map_err(|_| invalid("recovery name is too long"))?.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    Ok(())
}
fn take<'a>(bytes: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
    if bytes.len() < len { return Err(invalid("truncated recovery record")); }
    let (value, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(value)
}
fn take_identity(bytes: &mut &[u8]) -> io::Result<FileIdentity> {
    let device = u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap());
    let inode = u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap());
    Ok(FileIdentity { device, inode })
}
fn validate_name(name: &[u8]) -> io::Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&0) || name.contains(&b'/') { return Err(invalid("invalid recovery path component")); }
    Ok(())
}
fn take_name(bytes: &mut &[u8]) -> io::Result<OsString> {
    let len = u32::from_le_bytes(take(bytes, 4)?.try_into().unwrap()) as usize;
    let name = take(bytes, len)?;
    validate_name(name)?;
    Ok(OsString::from_vec(name.to_vec()))
}
fn kind_byte(kind: EntryType) -> io::Result<u8> {
    match kind { EntryType::RegularFile => Ok(0), EntryType::Directory => Ok(1), EntryType::Symlink => Ok(2), EntryType::Other => Ok(3), EntryType::Root => Err(invalid("invalid captured root type")) }
}
fn take_kind(bytes: &mut &[u8]) -> io::Result<EntryType> {
    match take(bytes, 1)?[0] { 0 => Ok(EntryType::RegularFile), 1 => Ok(EntryType::Directory), 2 => Ok(EntryType::Symlink), 3 => Ok(EntryType::Other), _ => Err(invalid("unknown captured object type")) }
}

impl Intent {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = INTENT_MAGIC.to_vec();
        put_identity(&mut bytes, self.root);
        bytes.extend_from_slice(&u32::try_from(self.parents.len()).map_err(|_| invalid("recovery path is too deep"))?.to_le_bytes());
        for parent in &self.parents { put_name(&mut bytes, &parent.name)?; put_identity(&mut bytes, parent.identity); }
        put_name(&mut bytes, &self.name)?;
        bytes.push(match &self.kind { RecordKind::Retirement => 0, RecordKind::Stage => 1, RecordKind::Replacement(_) => 2 });
        match (&self.kind, self.expected) {
            (RecordKind::Retirement | RecordKind::Replacement(_), Some(expected)) => {
                put_identity(&mut bytes, expected.file);
                bytes.push(kind_byte(expected.kind)?);
            },
            (RecordKind::Stage, None) => {},
            _ => return Err(invalid("recovery record kind and expected object disagree")),
        }
        if let RecordKind::Replacement(publication) = &self.kind {
            put_identity(&mut bytes, publication.source.file);
            bytes.push(kind_byte(publication.source.kind)?);
            put_identity(&mut bytes, publication.destination_parent);
            put_name(&mut bytes, &publication.destination_name)?;
        }
        if bytes.len() > MAX_RECORD { return Err(invalid("recovery record exceeds bound")); }
        Ok(bytes)
    }

    fn decode(mut bytes: &[u8], legacy: bool) -> io::Result<Self> {
        let magic = if legacy { LEGACY_INTENT_MAGIC } else { INTENT_MAGIC };
        if take(&mut bytes, magic.len())? != magic { return Err(invalid("unknown recovery intent version")); }
        let root = take_identity(&mut bytes)?;
        let count = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
        if count > bytes.len() / 21 { return Err(invalid("invalid recovery ancestor count")); }
        let mut parents = Vec::with_capacity(count);
        for _ in 0..count { parents.push(Ancestor { name: take_name(&mut bytes)?, identity: take_identity(&mut bytes)? }); }
        let name = take_name(&mut bytes)?;
        let (kind, expected) = if legacy { (RecordKind::Retirement, None) } else {
            let tag = take(&mut bytes, 1)?[0];
            if tag > 2 { return Err(invalid("unknown recovery record kind")); }
            let expected = if tag != 1 {
                let file = take_identity(&mut bytes)?;
                Some(EntryIdentity { file, kind: take_kind(&mut bytes)? })
            } else { None };
            let kind = match tag {
                0 => RecordKind::Retirement,
                1 => RecordKind::Stage,
                2 => {
                    let file = take_identity(&mut bytes)?;
                    let source = EntryIdentity { file, kind: take_kind(&mut bytes)? };
                    RecordKind::Replacement(Publication::new(source, take_identity(&mut bytes)?, take_name(&mut bytes)?)?)
                },
                _ => unreachable!(),
            };
            (kind, expected)
        };
        if !bytes.is_empty() { return Err(invalid("trailing recovery record data")); }
        Ok(Self { root, parents, name, expected, kind })
    }

    fn parent(&self, root: BorrowedFd<'_>, mount: &MountIdentity) -> io::Result<OwnedFd> {
        let (found, found_mount) = directory_state(root)?;
        if found != self.root || found_mount != *mount { return Err(invalid("recovery root identity changed")); }
        let mut fd = retry(|| fs::openat(root, c".", DIRECTORY_FLAGS, Mode::empty()))?;
        for parent in &self.parents {
            let next = retry(|| fs::openat(&fd, &parent.name, DIRECTORY_FLAGS, Mode::empty()))?;
            let (found, found_mount) = directory_state(next.as_fd())?;
            if found != parent.identity || found_mount != *mount { return Err(invalid("recovery parent identity or mount changed")); }
            fd = next;
        }
        Ok(fd)
    }
}

fn retained(workspace: &OsStr, capture: Option<&OsStr>, error: io::Error) -> io::Error {
    let mut location = std::path::PathBuf::from(workspace);
    if let Some(capture) = capture { location.push(capture); }
    io::Error::new(error.kind(), format!("{error}; retained at scan root/{location:?}; restore without overwriting the original name, then rescan"))
}

fn lock(root: BorrowedFd<'_>) -> io::Result<RootLock> {
    let fd = retry(|| fs::openat(root, c".", DIRECTORY_FLAGS, Mode::empty()))?;
    fs::flock(&fd, fs::FlockOperation::NonBlockingLockExclusive).map_err(|error| io::Error::new(io::Error::from(error).kind(), "another deletion or recovery owns this root"))?;
    Ok(RootLock { directory: fs::Dir::new(fd)?, owner: rustix::process::getpid() })
}

fn validate_directory(fd: BorrowedFd<'_>, legacy: bool) -> io::Result<()> {
    let stat = retry(|| fs::fstat(fd))?;
    if identity::entry_type(&stat) != EntryType::Directory || stat.st_uid != rustix::process::geteuid().as_raw() || (!legacy && stat.st_mode & 0o077 != 0) { return Err(invalid("quarantine directory owner, mode, or type is invalid")); }
    Ok(())
}

fn visit_root_names(root: &mut RootLock, mut visit: impl FnMut(BorrowedFd<'_>, OsString) -> io::Result<()>) -> io::Result<()> {
    while let Some(entry) = root.directory.next() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." { visit(root.as_fd(), OsString::from_vec(name.to_vec()))?; }
    }
    Ok(())
}

fn recover(root: &mut RootLock) -> io::Result<bool> {
    let (root_identity, mount) = directory_state(root.as_fd())?;
    let mut recovered = false;
    visit_root_names(root, |root, name| {
        let bytes = name.as_bytes();
        if bytes.starts_with(b".ish-fs-stage-") || bytes.starts_with(b".ish-fs-retired-") || bytes.starts_with(b".ish-fs-intent-") { return Err(retained(&name, None, invalid("legacy ish artifact requires inspection; ownership is unknown"))); }
        if !bytes.starts_with(b".fdu-quarantine-") { return Ok(()); }
        let legacy = bytes.starts_with(LEGACY_PREFIX);
        if !legacy && !bytes.starts_with(PREFIX) { return Err(retained(&name, None, invalid("unknown quarantine version"))); }
        let suffix = &bytes[if legacy { LEGACY_PREFIX.len() } else { PREFIX.len() }..];
        if suffix.len() != 32 || !suffix.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) { return Err(retained(&name, None, invalid("invalid quarantine session name"))); }
        let directory = retry(|| fs::openat(root, &name, DIRECTORY_FLAGS, Mode::empty())).map_err(|error| retained(&name, None, error))?;
        validate_directory(directory.as_fd(), legacy).map_err(|error| retained(&name, None, error))?;
        let header = read(directory.as_fd(), OsStr::new("header"), legacy).map_err(|error| retained(&name, None, io::Error::new(error.kind(), format!("ambiguous quarantine directory: {error}"))))?;
        let magic = if legacy { LEGACY_MAGIC } else { MAGIC };
        if !header.starts_with(magic) { return Err(retained(&name, None, invalid("unknown or damaged quarantine header"))); }
        let mut data = &header[magic.len()..];
        let recorded_root = take_identity(&mut data).map_err(|error| retained(&name, None, error))?;
        if !legacy {
            let uid = u32::from_le_bytes(take(&mut data, 4)?.try_into().unwrap());
            let session = take_identity(&mut data)?;
            let recorded_mount = take_mount(&mut data)?;
            if recorded_mount != mount { return Err(retained(&name, None, invalid("quarantine mount identity changed"))); }
            if uid != rustix::process::geteuid().as_raw() || session != directory_state(directory.as_fd())?.0 { return Err(retained(&name, None, invalid("quarantine header owner or session identity changed"))); }
        }
        if recorded_root != root_identity || !data.is_empty() || directory_state(directory.as_fd())?.1 != mount { return Err(retained(&name, None, invalid("quarantine root identity, format, or mount changed"))); }
        recover_directory(root, directory.as_fd(), &name, &mount, legacy)?;
        fs::unlinkat(&directory, c"header", AtFlags::empty())?;
        sync(directory.as_fd())?;
        fs::unlinkat(root, &name, AtFlags::REMOVEDIR)?;
        sync(root)?;
        recovered = true;
        Ok(())
    })?;
    Ok(recovered)
}

fn recover_directory(root: BorrowedFd<'_>, directory: BorrowedFd<'_>, workspace: &OsStr, mount: &MountIdentity, legacy: bool) -> io::Result<()> {
    visit_names(directory, |name| {
        let bytes = name.as_bytes();
        if bytes == b"header" { return Ok(()); }
        if bytes.starts_with(b"pending-") {
            if !legacy { return Err(retained(workspace, Some(&name), invalid("unpublished recovery record requires inspection"))); }
            read(directory, &name, true).map_err(|error| retained(workspace, Some(&name), error))?;
            fs::unlinkat(directory, &name, AtFlags::empty())?;
            return sync(directory);
        }
        if let Some(id) = bytes.strip_suffix(b".entry") {
            match identity::stat_at(directory, &name) {
                Err(Errno::NOENT) => return Ok(()),
                Err(error) => return Err(retained(workspace, Some(&name), error.into())),
                Ok(_) => {},
            }
            let mut record = id.to_vec(); record.extend_from_slice(b".intent");
            if identity::stat_at(directory, OsStr::from_bytes(&record)).is_ok() { return Ok(()); }
            return Err(retained(workspace, Some(&name), invalid("unrecognized quarantine entry")));
        }
        let id = bytes.strip_suffix(b".intent").ok_or_else(|| retained(workspace, Some(&name), invalid("unrecognized quarantine entry")))?;
        if id.is_empty() || !id.iter().all(u8::is_ascii_digit) { return Err(retained(workspace, Some(&name), invalid("invalid capture identifier"))); }
        let capture = OsString::from_vec([id, b".entry"].concat());
        let intent = Intent::decode(&read(directory, &name, legacy).map_err(|error| retained(workspace, Some(&name), error))?, legacy).map_err(|error| retained(workspace, Some(&name), error))?;
        let parent = intent.parent(root, mount).map_err(|error| retained(workspace, Some(&capture), error))?;
        match identity::stat_at(directory, &capture) {
            Ok(stat) => {
                if intent.kind == RecordKind::Stage { return Err(retained(workspace, Some(&capture), invalid("unpublished staging object requires inspection"))); }
                if let Some(expected) = intent.expected
                    && identity::entry_identity(&stat)? != expected { return Err(retained(workspace, Some(&capture), invalid("captured object identity or kind changed"))); }
                fs::renameat_with(directory, &capture, &parent, &intent.name, fs::RenameFlags::NOREPLACE).map_err(|error| retained(workspace, Some(&capture), error.into()))?;
                sync(parent.as_fd()).map_err(|error| retained(workspace, None, error))?;
                sync(directory).map_err(|error| retained(workspace, None, error))?;
            }
            Err(Errno::NOENT) => {
                if let RecordKind::Replacement(publication) = &intent.kind {
                    let source = identity::stat_at(parent.as_fd(), &intent.name).and_then(|stat| identity::entry_identity(&stat).map_err(|_| Errno::IO));
                    if source != Ok(publication.source) {
                        return Err(retained(workspace, Some(&name), invalid(&format!("replacement interrupted before capture; displaced name {:?}, destination parent {:?}, destination name {:?}; publication is not replayed", intent.name, publication.destination_parent, publication.destination_name))));
                    }
                }
                sync(parent.as_fd())?; sync(directory)?;
            }
            Err(error) => return Err(retained(workspace, Some(&capture), error.into())),
        }
        fs::unlinkat(directory, &name, AtFlags::empty())?;
        sync(directory)
    })
}

fn random_bytes() -> io::Result<[u8; 16]> {
    let mut random = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random)
}

fn create_session_directory(root: BorrowedFd<'_>, mut random: impl FnMut() -> io::Result<[u8; 16]>) -> io::Result<OsString> {
    for _ in 0..16 {
        let mut name = PREFIX.to_vec();
        for byte in random()? { name.extend_from_slice(format!("{byte:02x}").as_bytes()); }
        let name = OsString::from_vec(name);
        match fs::mkdirat(root, &name, Mode::RWXU) {
            Ok(()) => return Ok(name),
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "quarantine random name collision limit reached"))
}

#[cfg(target_os = "macos")]
fn check_creation_mask() -> io::Result<()> {
    // SAFETY: getumask is a public no-argument mode_t function on macOS 27 and later; lookup preserves support for older hosts.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"getumask".as_ptr()) };
    if !symbol.is_null() {
        // SAFETY: the non-null symbol names getumask with its documented C signature.
        let getumask: unsafe extern "C" fn() -> libc::mode_t = unsafe { std::mem::transmute(symbol) };
        // SAFETY: getumask reads the process mask without changing process state.
        if unsafe { getumask() } & 0o500 != 0 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "macOS filesystem staging does not support a restrictive umask that removes owner read or search permission"));
        }
    }
    Ok(())
}

fn open_created_directory(root: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let held = retry(|| fs::openat(root, name, OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty()))?;
    #[cfg(target_os = "macos")]
    let held = {
        let stat = identity::stat_at(root, name)?;
        if identity::entry_type(&stat) != EntryType::Directory || stat.st_uid != rustix::process::geteuid().as_raw() {
            return Err(invalid("quarantine directory owner or type is invalid"));
        }
        if stat.st_mode & 0o500 != 0o500 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "macOS filesystem staging does not support a restrictive umask that removes owner read or search permission"));
        }
        use rustix::fd::{AsRawFd, FromRawFd};
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid("invalid session name"))?;
        // SAFETY: the borrowed descriptor is live and the name is NUL-terminated.
        let raw = unsafe { libc::openat(root.as_raw_fd(), name.as_ptr(), libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if raw == -1 { return Err(io::Error::last_os_error()); }
        // SAFETY: openat returned a new owned descriptor.
        unsafe { OwnedFd::from_raw_fd(raw) }
    };
    validate_directory(held.as_fd(), false)?;
    #[cfg(target_os = "linux")]
    {
        use rustix::fd::AsRawFd;
        let path = format!("/proc/self/fd/{}", held.as_raw_fd());
        retry(|| fs::chmodat(root, path.as_str(), Mode::RWXU, AtFlags::empty()))?;
    }
    #[cfg(target_os = "macos")]
    retry(|| fs::fchmod(&held, Mode::RWXU))?;
    let directory = retry(|| fs::openat(&held, c".", DIRECTORY_FLAGS, Mode::empty()))?;
    if directory_state(directory.as_fd())?.0 != directory_state(held.as_fd())?.0 { return Err(invalid("session identity changed while opening")); }
    Ok(directory)
}

impl Session {
    pub fn start(root: BorrowedFd<'_>) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        check_creation_mask()?;
        let mut root = lock(root)?;
        if recover(&mut root)? { return Err(io::Error::other("interrupted deletion recovered; rescan before deleting")); }
        let mount = identity::mount_identity(root.as_fd())?;
        let name = create_session_directory(root.as_fd(), random_bytes)?;
        let directory = open_created_directory(root.as_fd(), &name).map_err(|error| retained(&name, None, error))?;
        let initialized = (|| {
            fs::fchmod(&directory, Mode::RWXU)?;
            validate_directory(directory.as_fd(), false)?;
            let (session_identity, session_mount) = directory_state(directory.as_fd())?;
            if session_mount != mount { return Err(invalid("quarantine is on another mount")); }
            let mut header = MAGIC.to_vec();
            put_identity(&mut header, directory_state(root.as_fd())?.0);
            header.extend_from_slice(&rustix::process::geteuid().as_raw().to_le_bytes());
            put_identity(&mut header, session_identity);
            put_mount(&mut header, &mount)?;
            publish(directory.as_fd(), OsStr::new("pending-header"), OsStr::new("header"), &header)?;
            sync(root.as_fd())
        })();
        if let Err(error) = initialized { return Err(retained(&name, None, error)); }
        Ok(Self { owner: rustix::process::getpid(), root, directory, name, mount, next: AtomicU64::new(0), recovery_required: AtomicBool::new(false) })
    }

    fn require_owner(&self) -> io::Result<()> {
        if rustix::process::getpid() != self.owner { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "quarantine session belongs to another process")); }
        Ok(())
    }

    pub fn check_recovery(root: BorrowedFd<'_>) -> io::Result<()> {
        let mut root = lock(root)?;
        if recover(&mut root)? { return Err(io::Error::other("interrupted deletion recovered; rescan before deleting")); }
        Ok(())
    }

    pub fn directory(&self) -> BorrowedFd<'_> { self.directory.as_fd() }
    pub fn name(&self) -> &OsStr { &self.name }
    pub fn recovery_required(&self) -> bool { self.recovery_required.load(Ordering::Relaxed) }

    pub fn stage_name(&self) -> OsString {
        OsString::from(format!("{}.entry", self.next.fetch_add(1, Ordering::Relaxed)))
    }

    pub fn register_stage(&self, name: &OsStr) -> io::Result<OsString> {
        self.require_owner()?;
        let id = name.as_bytes().strip_suffix(b".entry").ok_or_else(|| invalid("stage name is not a journal entry"))?;
        if id.is_empty() || !id.iter().all(u8::is_ascii_digit) { return Err(invalid("invalid stage identifier")); }
        let record = OsString::from_vec([id, b".intent"].concat());
        let pending = OsString::from_vec([b"pending-", id].concat());
        let intent = Intent { root: directory_state(self.root.as_fd())?.0, parents: Vec::new(), name: name.to_os_string(), expected: None, kind: RecordKind::Stage };
        publish(self.directory.as_fd(), &pending, &record, &intent.encode()?)?;
        Ok(record)
    }

    pub fn clear_stage(&self, record: &OsStr) -> io::Result<()> { self.require_owner()?; self.clear_record(record) }

    pub fn remove(&self, parent: BorrowedFd<'_>, request: Retirement, mut authorize: impl FnMut(Boundary, BorrowedFd<'_>, &OsStr) -> io::Result<()>) -> Result<Removal, RemovalError> {
        self.remove_with_publication(parent, request, None, || Ok(()), &mut authorize)
    }

    pub fn remove_after_publication(&self, parent: BorrowedFd<'_>, request: Retirement, publication: Publication, publish: impl FnOnce() -> io::Result<()>, mut authorize: impl FnMut(Boundary, BorrowedFd<'_>, &OsStr) -> io::Result<()>) -> Result<Removal, RemovalError> {
        self.remove_with_publication(parent, request, Some(publication), publish, &mut authorize)
    }

    fn remove_with_publication(&self, parent: BorrowedFd<'_>, request: Retirement, publication: Option<Publication>, publish_entry: impl FnOnce() -> io::Result<()>, authorize: &mut impl FnMut(Boundary, BorrowedFd<'_>, &OsStr) -> io::Result<()>) -> Result<Removal, RemovalError> {
        self.require_owner().map_err(|error| RemovalError { removed: false, error })?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let capture = OsString::from(format!("{id}.entry"));
        let record = OsString::from(format!("{id}.intent"));
        let pending = OsString::from(format!("pending-{id}"));
        let before = publication.as_ref().map_or(request.expected, |publication| publication.source);
        let intent = Intent { root: directory_state(self.root.as_fd()).map_err(|error| RemovalError { removed: false, error })?.0, parents: request.parents, name: request.name, expected: Some(request.expected), kind: publication.map_or(RecordKind::Retirement, RecordKind::Replacement) };
        let mut captured = false;
        let mut removed = false;
        let result = (|| {
            let checked = intent.parent(self.root.as_fd(), &self.mount)?;
            if directory_state(checked.as_fd())? != directory_state(parent)? { return Err(invalid("source parent identity or mount changed")); }
            match identity::stat_at(parent, &intent.name) {
                Ok(stat) if identity::entry_identity(&stat)? != before => return Ok(Removal::Changed),
                Ok(_) => {},
                Err(Errno::NOENT) => return Ok(Removal::AlreadyAbsent),
                Err(error) => return Err(error.into()),
            }
            publish(self.directory.as_fd(), &pending, &record, &intent.encode()?)?;
            authorize(Boundary::IntentPublished, parent, &intent.name)?;
            if matches!(&intent.kind, RecordKind::Replacement(_)) {
                if identity::entry_identity(&identity::stat_at(parent, &intent.name)?)? != before { return Err(invalid("replacement source changed before publication")); }
                publish_entry()?;
                authorize(Boundary::Published, parent, &intent.name)?;
            }
            authorize(Boundary::BeforeCapture, parent, &intent.name)?;
            let checked = intent.parent(self.root.as_fd(), &self.mount)?;
            if directory_state(checked.as_fd())? != directory_state(parent)? { return Err(invalid("source parent identity or mount changed before capture")); }
            fs::renameat_with(parent, &intent.name, &self.directory, &capture, fs::RenameFlags::NOREPLACE)?;
            captured = true;
            authorize(Boundary::Captured, self.directory.as_fd(), &capture)?;
            sync(parent)?;
            authorize(Boundary::CaptureParentSynced, self.directory.as_fd(), &capture)?;
            sync(self.directory.as_fd())?;
            authorize(Boundary::CaptureDirectorySynced, self.directory.as_fd(), &capture)?;
            if identity::entry_identity(&identity::stat_at(self.directory.as_fd(), &capture)?)? != request.expected {
                let checked = intent.parent(self.root.as_fd(), &self.mount)?;
                self.restore(checked.as_fd(), &intent.name, &capture, &record)?;
                captured = false;
                return Ok(Removal::Changed);
            }
            authorize(Boundary::BeforeUnlink, self.directory.as_fd(), &capture)?;
            let checked = intent.parent(self.root.as_fd(), &self.mount)?;
            if directory_state(checked.as_fd())? != directory_state(parent)? { return Err(invalid("source parent identity or mount changed before removal")); }
            if identity::entry_identity(&identity::stat_at(self.directory.as_fd(), &capture)?)? != request.expected { return Err(invalid("captured object changed before removal")); }
            fs::unlinkat(&self.directory, &capture, if request.expected.kind == EntryType::Directory { AtFlags::REMOVEDIR } else { AtFlags::empty() })?;
            removed = true;
            authorize(Boundary::Unlinked, self.directory.as_fd(), &capture)?;
            sync(self.directory.as_fd())?;
            authorize(Boundary::RemovalDirectorySynced, self.directory.as_fd(), &capture)?;
            authorize(Boundary::BeforeParentSync, parent, &intent.name)?;
            sync(parent)?;
            authorize(Boundary::ParentSynced, parent, &intent.name)?;
            fs::unlinkat(&self.directory, &record, AtFlags::empty())?;
            authorize(Boundary::IntentRemoved, parent, &intent.name)?;
            sync(self.directory.as_fd())?;
            authorize(Boundary::IntentRemovalSynced, parent, &intent.name)?;
            Ok(Removal::Removed)
        })();
        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                let error = if error.raw_os_error() == Some(Errno::NOTEMPTY.raw_os_error()) { io::Error::new(error.kind(), format!("directory gained unindexed children: {error}")) } else { error };
                let error = if captured && !removed {
                    match intent.parent(self.root.as_fd(), &self.mount).and_then(|checked| {
                        self.restore(checked.as_fd(), &intent.name, &capture, &record)?;
                        authorize(Boundary::Restored, checked.as_fd(), &intent.name).map_err(|verification| io::Error::new(verification.kind(), format!("source restored; restoration verification failed: {verification}")))
                    }) {
                        Ok(()) => error,
                        Err(restoration) => io::Error::new(error.kind(), format!("{error}; restoration failed: {restoration}")),
                    }
                } else if !captured && !removed && (!matches!(&intent.kind, RecordKind::Replacement(_)) || identity::stat_at(parent, &intent.name).ok().and_then(|stat| identity::entry_identity(&stat).ok()) == Some(before)) && identity::stat_at(self.directory.as_fd(), &record).is_ok() {
                    match self.clear_record(&record) { Ok(()) => error, Err(cleanup) => io::Error::new(error.kind(), format!("{error}; intent cleanup failed: {cleanup}")) }
                } else { error };
                let record_exists = identity::stat_at(self.directory.as_fd(), &record).is_ok();
                let pending_exists = identity::stat_at(self.directory.as_fd(), &pending).is_ok();
                let unresolved = record_exists || pending_exists;
                let error = if unresolved {
                    self.recovery_required.store(true, Ordering::Relaxed);
                    let location = if identity::stat_at(self.directory.as_fd(), &capture).is_ok() { &capture } else if record_exists { &record } else { &pending };
                    retained(&self.name, Some(location), error)
                } else { error };
                Err(RemovalError { removed, error })
            }
        }
    }

    fn restore(&self, parent: BorrowedFd<'_>, name: &OsStr, capture: &OsStr, record: &OsStr) -> io::Result<()> {
        fs::renameat_with(&self.directory, capture, parent, name, fs::RenameFlags::NOREPLACE)?;
        sync(parent)?;
        sync(self.directory.as_fd())?;
        self.clear_record(record)
    }

    fn clear_record(&self, record: &OsStr) -> io::Result<()> {
        fs::unlinkat(&self.directory, record, AtFlags::empty())?;
        sync(self.directory.as_fd())
    }

    pub fn finish(&self) -> io::Result<()> {
        self.require_owner()?;
        let mut occupied = false;
        visit_names(self.directory.as_fd(), |name| { if name != "header" { occupied = true; } Ok(()) })?;
        if occupied { return Err(retained(&self.name, None, invalid("unresolved journal entries require recovery"))); }
        fs::unlinkat(&self.directory, c"header", AtFlags::empty())?;
        sync(self.directory.as_fd())?;
        fs::unlinkat(&self.root, &self.name, AtFlags::REMOVEDIR)?;
        sync(self.root.as_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn inaccessible_created_directory_is_rejected_without_changing_permissions() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("ish45-inaccessible-directory-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let root = std::fs::File::open(&path).unwrap();
        for mode in [0o300, 0o600, 0] {
            fs::mkdirat(&root, c"stage", Mode::from_raw_mode(mode)).unwrap();
            let before = identity::stat_at(root.as_fd(), OsStr::new("stage")).unwrap();
            let error = open_created_directory(root.as_fd(), OsStr::new("stage")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            let after = identity::stat_at(root.as_fd(), OsStr::new("stage")).unwrap();
            assert_eq!(identity::entry_identity(&before).unwrap(), identity::entry_identity(&after).unwrap());
            assert_eq!(after.st_mode, before.st_mode);
            fs::unlinkat(&root, c"stage", AtFlags::REMOVEDIR).unwrap();
        }
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn random_name_collisions_are_bounded_and_never_replace_existing_entries() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root_path = base.join(format!("ish32-random-collisions-{}", std::process::id()));
        std::fs::create_dir(&root_path).unwrap();
        let root = std::fs::File::open(&root_path).unwrap();
        let occupied = format!("{}{}", std::str::from_utf8(PREFIX).unwrap(), "00".repeat(16));
        std::fs::create_dir(root_path.join(&occupied)).unwrap();
        std::fs::write(root_path.join(&occupied).join("sentinel"), b"unowned").unwrap();
        let mut attempts = 0;
        let created = create_session_directory(root.as_fd(), || { attempts += 1; Ok(if attempts == 1 { [0; 16] } else { [1; 16] }) }).unwrap();
        assert_eq!(attempts, 2);
        assert!(root_path.join(created).is_dir());
        let mut attempts = 0;
        let error = create_session_directory(root.as_fd(), || { attempts += 1; Ok([0; 16]) }).unwrap_err();
        assert_eq!(attempts, 16);
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(root_path.join(&occupied).join("sentinel")).unwrap(), b"unowned");
        std::fs::remove_dir_all(root_path).unwrap();
    }

    #[test]
    fn fork_child_cleanup_cannot_unlock_the_parent_session() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("ish32-owner-lock-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let root = std::fs::File::open(&path).unwrap();
        let session = Session::start(root.as_fd()).unwrap();
        session.finish().unwrap();
        let (mut parent, child) = UnixStream::pair().unwrap();
        parent.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let process = std::thread::spawn(move || {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--list"]).stdout(std::process::Stdio::null());
            // SAFETY: the child hook uses only PID inspection, flock, and descriptor I/O before exec.
            unsafe {
                command.pre_exec(move || {
                    session.root.release()?;
                    rustix::io::write(child.as_fd(), b"ready")?;
                    let mut release = [0u8; 1];
                    rustix::io::read(child.as_fd(), &mut release)?;
                    Ok(())
                });
            }
            command.spawn().unwrap().wait().unwrap()
        });
        let mut ready = [0u8; 5];
        parent.read_exact(&mut ready).unwrap();
        let contender = Session::start(root.as_fd());
        parent.write_all(b"x").unwrap();
        assert!(process.join().unwrap().success());
        if let Ok(session) = &contender { session.finish().unwrap(); }
        assert!(contender.is_err(), "a fork child released its parent's session lock");
        let next = Session::start(root.as_fd()).unwrap();
        next.finish().unwrap();
        drop(next);
        std::fs::remove_dir_all(path).unwrap();
    }

}
