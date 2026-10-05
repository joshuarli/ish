use super::legacy;
use std::fs::{self, DirBuilder, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const HEADER: &[u8; 8] = b"ISHLOG\0\x01";
const HEADER_SIZE: u64 = 16;
const FRAME_HEADER_SIZE: u64 = 16;

pub(super) struct Occurrence {
    pub id: i64,
    pub command: String,
    pub timestamp: u64,
    pub session_id: u64,
    pub cwd: Option<PathBuf>,
}

pub(super) struct Snapshot {
    pub generation: i64,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Clone, Copy)]
struct Cursor {
    identity: Option<(u64, u64)>,
    generation: i64,
    offset: u64,
    last_id: i64,
}

impl Default for Cursor {
    fn default() -> Self {
        Self { identity: None, generation: 0, offset: HEADER_SIZE, last_id: 0 }
    }
}

pub(super) struct Store {
    pub path: PathBuf,
    cursor: Cursor,
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

pub(super) fn storage_path(path: &Path) -> PathBuf {
    suffixed_path(path, ".log")
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("history log: {message}"))
}

fn open_file(path: &Path, flags: rustix::fs::OFlags) -> io::Result<File> {
    let fd = rustix::fs::open(path, flags | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600))?;
    Ok(File::from(fd))
}

// Each operation opens its own lock descriptor and drops it before user commands
// can fork. The lock inode survives atomic replacement of the data file.
fn lock(path: &Path) -> io::Result<File> {
    let file = open_file(&suffixed_path(path, ".lock"),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE)?;
    loop {
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
            Ok(()) => return Ok(file),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

impl Store {
    pub fn open(legacy_path: &Path) -> io::Result<Self> {
        let path = storage_path(legacy_path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        let _lock = lock(&path)?;
        match fs::metadata(&path) {
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::metadata(suffixed_path(legacy_path, ".sqlite3")) {
                    Ok(_) => return Err(io::Error::other(
                        "SQLite history exists; run scripts/migrate-sqlite-history before opening ish")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error),
                }
                let mut occurrences = legacy::read(legacy_path)?;
                for (index, occurrence) in occurrences.iter_mut().enumerate() {
                    occurrence.id = i64::try_from(index).ok().and_then(|id| id.checked_add(1))
                        .ok_or_else(|| invalid("too many occurrences"))?;
                }
                replace(&path, 0, &occurrences)?;
            },
            Err(error) => return Err(error),
        }
        Ok(Self { path, cursor: Cursor::default() })
    }

    pub fn snapshot(&mut self, generation: i64, last_id: i64) -> io::Result<Snapshot> {
        let _lock = lock(&self.path)?;
        let mut file = open_file(&self.path, rustix::fs::OFlags::RDWR)?;
        let mut cursor = self.cursor;
        let snapshot = read_snapshot(&mut file, &mut cursor, generation, last_id)?;
        self.cursor = cursor;
        Ok(snapshot)
    }

    pub fn append(&mut self, generation: i64, last_id: i64, mut occurrence: Occurrence)
        -> io::Result<(Snapshot, i64)> {
        let _lock = lock(&self.path)?;
        let mut file = open_file(&self.path, rustix::fs::OFlags::RDWR)?;
        let mut cursor = self.cursor;
        let mut snapshot = read_snapshot(&mut file, &mut cursor, generation, last_id)?;
        let id = cursor.last_id.checked_add(1).ok_or_else(|| invalid("occurrence id exhausted"))?;
        occurrence.id = id;
        let frame = encode(&occurrence)?;
        let next_offset = cursor.offset.checked_add(frame.len() as u64)
            .ok_or_else(|| invalid("file offset overflow"))?;
        file.seek(SeekFrom::Start(cursor.offset))?;
        // Publish the occurrence to memory only after both its bytes and metadata
        // are durable. A failed write must leave the valid prefix retryable.
        if let Err(error) = file.write_all(&frame).and_then(|_| file.sync_all()) {
            if let Err(rollback) = file.set_len(cursor.offset).and_then(|_| file.sync_all()) {
                return Err(io::Error::other(format!(
                    "history append failed: {error}; tail rollback failed: {rollback}")));
            }
            return Err(error);
        }
        cursor.offset = next_offset;
        cursor.last_id = id;
        self.cursor = cursor;
        snapshot.occurrences.push(occurrence);
        Ok((snapshot, id))
    }

    pub fn reset(&mut self) -> io::Result<i64> {
        let _lock = lock(&self.path)?;
        let mut file = open_file(&self.path, rustix::fs::OFlags::RDWR)?;
        let mut cursor = Cursor::default();
        let snapshot = read_snapshot(&mut file, &mut cursor, -1, 0)?;
        let generation = snapshot.generation.checked_add(1)
            .ok_or_else(|| invalid("generation exhausted"))?;
        replace(&self.path, generation, &[])?;
        self.cursor = Cursor::default();
        Ok(generation)
    }

    pub fn compact(&mut self) -> io::Result<()> {
        let _lock = lock(&self.path)?;
        let mut file = open_file(&self.path, rustix::fs::OFlags::RDWR)?;
        let mut cursor = Cursor::default();
        let snapshot = read_snapshot(&mut file, &mut cursor, -1, 0)?;
        // Rewriting preserves every occurrence, its id, and its generation;
        // another shell can keep its recall order and synchronize by id.
        replace(&self.path, snapshot.generation, &snapshot.occurrences)
    }
}

fn read_snapshot(file: &mut File, cursor: &mut Cursor, generation: i64, last_id: i64)
    -> io::Result<Snapshot> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; HEADER_SIZE as usize];
    file.read_exact(&mut header).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof { invalid("incomplete file header") }
        else { error }
    })?;
    if &header[..8] != HEADER { return Err(invalid("unsupported or corrupt file header")); }
    let current = i64::from_le_bytes(header[8..].try_into().unwrap());
    if current < 0 { return Err(invalid("negative generation")); }
    let metadata = file.metadata()?;
    let identity = (metadata.dev(), metadata.ino());
    if cursor.identity != Some(identity) || cursor.generation != current
        || metadata.len() < cursor.offset || last_id < cursor.last_id {
        *cursor = Cursor { identity: Some(identity), generation: current, ..Cursor::default() };
    }
    let cutoff = if current == generation { last_id } else { 0 };
    // A caller with an older generation needs the complete replacement snapshot.
    if current != generation && cursor.offset != HEADER_SIZE {
        cursor.offset = HEADER_SIZE;
        cursor.last_id = 0;
    }
    let mut occurrences = Vec::new();
    if cursor.offset == metadata.len() {
        return Ok(Snapshot { generation: current, occurrences });
    }
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut payload = Vec::new();
    while cursor.offset < metadata.len() {
        let remaining = metadata.len() - cursor.offset;
        if remaining < FRAME_HEADER_SIZE {
            recover_tail(reader.get_ref(), cursor.offset)?;
            break;
        }
        let mut frame_header = [0u8; FRAME_HEADER_SIZE as usize];
        reader.read_exact(&mut frame_header)?;
        let length_bytes: [u8; 8] = frame_header[..8].try_into().unwrap();
        let length = u64::from_le_bytes(length_bytes);
        let length_crc = u32::from_le_bytes(frame_header[8..12].try_into().unwrap());
        if crc32(&length_bytes) != length_crc { return Err(invalid("frame length checksum mismatch")); }
        if length < 32 { return Err(invalid("frame payload is too short")); }
        let frame_size = FRAME_HEADER_SIZE.checked_add(length)
            .ok_or_else(|| invalid("frame length overflow"))?;
        if frame_size > remaining {
            recover_tail(reader.get_ref(), cursor.offset)?;
            break;
        }
        let length = usize::try_from(length).map_err(|_| invalid("frame length exceeds address space"))?;
        payload.try_reserve_exact(length.saturating_sub(payload.len()))
            .map_err(|error| io::Error::other(error.to_string()))?;
        payload.resize(length, 0);
        reader.read_exact(&mut payload)?;
        let checksum = u32::from_le_bytes(frame_header[12..].try_into().unwrap());
        if crc32(&payload) != checksum { return Err(invalid("frame payload checksum mismatch")); }
        let occurrence = decode(&payload)?;
        if occurrence.id <= cursor.last_id { return Err(invalid("occurrence ids are not strictly increasing")); }
        cursor.last_id = occurrence.id;
        cursor.offset = cursor.offset.checked_add(frame_size).ok_or_else(|| invalid("file offset overflow"))?;
        if occurrence.id > cutoff { occurrences.push(occurrence); }
    }
    Ok(Snapshot { generation: current, occurrences })
}

fn recover_tail(file: &File, offset: u64) -> io::Result<()> {
    file.set_len(offset)?;
    file.sync_all()
}

fn decode(payload: &[u8]) -> io::Result<Occurrence> {
    let id = i64::from_le_bytes(payload[..8].try_into().unwrap());
    if id <= 0 { return Err(invalid("nonpositive occurrence id")); }
    let timestamp = u64::from_le_bytes(payload[8..16].try_into().unwrap());
    let session_id = u64::from_le_bytes(payload[16..24].try_into().unwrap());
    let cwd_len = u32::from_le_bytes(payload[24..28].try_into().unwrap());
    let command_len = u32::from_le_bytes(payload[28..32].try_into().unwrap()) as usize;
    let directory_size = if cwd_len == u32::MAX { 0 } else { cwd_len as usize };
    let command_start = 32usize.checked_add(directory_size).ok_or_else(|| invalid("directory length overflow"))?;
    if command_len == 0 || command_start.checked_add(command_len) != Some(payload.len()) {
        return Err(invalid("invalid payload field lengths"));
    }
    let command = std::str::from_utf8(&payload[command_start..])
        .map_err(|_| invalid("command is not UTF-8"))?.to_string();
    let cwd = (cwd_len != u32::MAX).then(|| PathBuf::from(
        std::ffi::OsString::from_vec(payload[32..command_start].to_vec())));
    Ok(Occurrence { id, timestamp, session_id, cwd, command })
}

fn encode(occurrence: &Occurrence) -> io::Result<Vec<u8>> {
    if occurrence.id <= 0 || occurrence.command.is_empty() { return Err(invalid("invalid occurrence")); }
    let cwd = occurrence.cwd.as_ref().map(|path| path.as_os_str().as_bytes());
    let cwd_len = match cwd {
        Some(bytes) => u32::try_from(bytes.len()).ok().filter(|&len| len != u32::MAX)
            .ok_or_else(|| invalid("directory is too long"))?,
        None => u32::MAX,
    };
    let command_len = u32::try_from(occurrence.command.len()).map_err(|_| invalid("command is too long"))?;
    let payload_len = 32usize.checked_add(cwd.map_or(0, <[u8]>::len))
        .and_then(|size| size.checked_add(occurrence.command.len())).ok_or_else(|| invalid("payload length overflow"))?;
    let length = u64::try_from(payload_len).map_err(|_| invalid("payload length overflow"))?.to_le_bytes();
    let frame_len = payload_len.checked_add(FRAME_HEADER_SIZE as usize).ok_or_else(|| invalid("frame length overflow"))?;
    let mut frame = Vec::new();
    frame.try_reserve_exact(frame_len).map_err(|error| io::Error::other(error.to_string()))?;
    frame.extend_from_slice(&length);
    frame.extend_from_slice(&crc32(&length).to_le_bytes());
    frame.extend_from_slice(&[0u8; 4]);
    frame.extend_from_slice(&occurrence.id.to_le_bytes());
    frame.extend_from_slice(&occurrence.timestamp.to_le_bytes());
    frame.extend_from_slice(&occurrence.session_id.to_le_bytes());
    frame.extend_from_slice(&cwd_len.to_le_bytes());
    frame.extend_from_slice(&command_len.to_le_bytes());
    if let Some(cwd) = cwd { frame.extend_from_slice(cwd); }
    frame.extend_from_slice(occurrence.command.as_bytes());
    let checksum = crc32(&frame[FRAME_HEADER_SIZE as usize..]);
    frame[12..16].copy_from_slice(&checksum.to_le_bytes());
    Ok(frame)
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 { (value >> 1) ^ 0xedb88320 } else { value >> 1 };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

pub(super) fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut crc = !0u32;
    for &byte in bytes { crc = (crc >> 8) ^ TABLE[((crc as u8) ^ byte) as usize]; }
    !crc
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

fn replace(path: &Path, generation: i64, occurrences: &[Occurrence]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temporary, file) = loop {
        let temporary = suffixed_path(path, &format!(".tmp-{}-{}", std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)));
        match open_file(&temporary, rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL) {
            Ok(file) => break (TemporaryFile(temporary), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    writer.write_all(HEADER)?;
    writer.write_all(&generation.to_le_bytes())?;
    for occurrence in occurrences { writer.write_all(&encode(occurrence)?)?; }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::rename(&temporary.0, path)?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    open_file(parent, rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY)?.sync_all()
}

pub(super) fn render(path: &Path) -> io::Result<String> {
    let _lock = lock(path)?;
    let mut file = open_file(path, rustix::fs::OFlags::RDWR)?;
    let snapshot = read_snapshot(&mut file, &mut Cursor::default(), -1, 0)?;
    let mut latest = fxhash::FxHashMap::default();
    for occurrence in snapshot.occurrences {
        let usage = latest.entry(occurrence.command).or_insert((0, 0));
        usage.0 = usage.0.max(occurrence.timestamp);
        usage.1 = usage.1.max(occurrence.id);
    }
    let mut entries: Vec<_> = latest.into_iter().collect();
    entries.sort_unstable_by_key(|(_, usage)| *usage);
    let mut output = String::new();
    for (command, _) in entries { output.push_str(&command); output.push('\n'); }
    Ok(output)
}
