use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{self, AtFlags, Mode, OFlags};

use super::unique_name;
use crate::roots::{EntryKind, Identity, Observation, Timestamp};

const MAX_RECORD: usize = 1024 * 1024;
const MAGIC: &[u8; 8] = b"ishfs\0\0\x01";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Record {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub observed: Observation,
    pub published: Identity,
    pub output: Option<Observation>,
    pub parent: Option<Identity>,
    pub anchor: Option<Identity>,
    pub ancestry: Vec<Identity>,
    pub final_flags: u32,
}

pub(super) struct Records {
    file: File,
    length: u64,
}

impl Records {
    pub fn new(scratch: &Path) -> io::Result<Self> {
        let directory = fs::open(scratch, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        let name = unique_name("records");
        let fd = fs::openat(&directory, &name, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR)?;
        fs::unlinkat(&directory, &name, AtFlags::empty())?;
        Ok(Self { file: fd.into(), length: 0 })
    }

    pub fn push(&mut self, record: &Record) -> io::Result<()> {
        let bytes = encode(record)?;
        self.file.seek(SeekFrom::Start(self.length))?;
        let length = u32::try_from(bytes.len()).map_err(|_| invalid("record exceeds limit"))?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&bytes)?;
        self.file.write_all(&length.to_le_bytes())?;
        self.length += bytes.len() as u64 + 8;
        Ok(())
    }

    pub fn next(&mut self, cursor: &mut u64) -> io::Result<Option<Record>> {
        if *cursor == self.length { return Ok(None); }
        if *cursor > self.length { return Err(invalid("record cursor exceeds spool")); }
        self.file.seek(SeekFrom::Start(*cursor))?;
        let length = read_length(&mut self.file)?;
        if u64::try_from(length).unwrap() + 8 > self.length - *cursor { return Err(invalid("truncated record")); }
        let mut bytes = vec![0; length];
        self.file.read_exact(&mut bytes)?;
        if read_length(&mut self.file)? != length { return Err(invalid("record length mismatch")); }
        *cursor += length as u64 + 8;
        decode(&bytes).map(Some)
    }

    pub fn previous(&mut self, cursor: &mut u64) -> io::Result<Option<Record>> {
        if *cursor == 0 { return Ok(None); }
        if *cursor < 8 || *cursor > self.length { return Err(invalid("invalid reverse cursor")); }
        self.file.seek(SeekFrom::Start(*cursor - 4))?;
        let length = read_length(&mut self.file)?;
        let size = length as u64 + 8;
        if size > *cursor { return Err(invalid("truncated reverse record")); }
        let start = *cursor - size;
        let mut forward = start;
        let record = self.next(&mut forward)?;
        if forward != *cursor { return Err(invalid("invalid reverse record length")); }
        *cursor = start;
        Ok(record)
    }

    pub fn end(&self) -> u64 { self.length }

    pub fn find(&mut self, path: &Path) -> io::Result<Option<Record>> {
        let mut cursor = 0;
        while let Some(record) = self.next(&mut cursor)? {
            if record.source == path { return Ok(Some(record)); }
        }
        Ok(None)
    }


}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn read_length(file: &mut File) -> io::Result<usize> {
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    let length = u32::from_le_bytes(bytes) as usize;
    if length > MAX_RECORD { return Err(invalid("record exceeds decoder limit")); }
    Ok(length)
}

fn put_identity(bytes: &mut Vec<u8>, value: Identity) {
    bytes.extend_from_slice(&value.device.to_le_bytes());
    bytes.extend_from_slice(&value.inode.to_le_bytes());
    bytes.push(match value.kind { EntryKind::RegularFile => 0, EntryKind::Directory => 1, EntryKind::Symlink => 2, EntryKind::Other => 3 });
}

fn put_path(bytes: &mut Vec<u8>, path: &Path) -> io::Result<()> {
    let path = path.as_os_str().as_bytes();
    let length = u32::try_from(path.len()).map_err(|_| invalid("record path exceeds limit"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(path);
    Ok(())
}

fn encode(record: &Record) -> io::Result<Vec<u8>> {
    let mut bytes = MAGIC.to_vec();
    put_path(&mut bytes, &record.source)?;
    put_path(&mut bytes, &record.destination)?;
    put_identity(&mut bytes, record.observed.identity);
    put_identity(&mut bytes, record.published);
    for value in [record.observed.size, record.observed.mode as u64, record.observed.uid as u64, record.observed.gid as u64, record.observed.links] { bytes.extend_from_slice(&value.to_le_bytes()); }
    for value in [record.observed.atime, record.observed.mtime, record.observed.ctime] {
        bytes.extend_from_slice(&value.seconds.to_le_bytes());
        bytes.extend_from_slice(&value.nanoseconds.to_le_bytes());
    }
    match record.output {
        Some(observation) => {
            bytes.push(1);
            put_identity(&mut bytes, observation.identity);
            for value in [observation.size, observation.mode as u64, observation.uid as u64, observation.gid as u64, observation.links] { bytes.extend_from_slice(&value.to_le_bytes()); }
            for value in [observation.atime, observation.mtime, observation.ctime] { bytes.extend_from_slice(&value.seconds.to_le_bytes()); bytes.extend_from_slice(&value.nanoseconds.to_le_bytes()); }
        },
        None => bytes.push(0),
    }
    match record.parent { Some(identity) => { bytes.push(1); put_identity(&mut bytes, identity); }, None => bytes.push(0) }
    match record.anchor { Some(identity) => { bytes.push(1); put_identity(&mut bytes, identity); }, None => bytes.push(0) }
    bytes.extend_from_slice(&u32::try_from(record.ancestry.len()).map_err(|_| invalid("ancestry exceeds limit"))?.to_le_bytes());
    for identity in &record.ancestry { put_identity(&mut bytes, *identity); }
    bytes.extend_from_slice(&record.final_flags.to_le_bytes());
    if bytes.len() > MAX_RECORD { return Err(invalid("record exceeds limit")); }
    Ok(bytes)
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if count > self.0.len() { return Err(invalid("truncated record field")); }
        let (value, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(value)
    }
    fn u64(&mut self) -> io::Result<u64> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    fn u32(&mut self) -> io::Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn identity(&mut self) -> io::Result<Identity> {
        let device = self.u64()?;
        let inode = self.u64()?;
        let kind = match self.take(1)?[0] { 0 => EntryKind::RegularFile, 1 => EntryKind::Directory, 2 => EntryKind::Symlink, 3 => EntryKind::Other, _ => return Err(invalid("unknown identity kind")) };
        Ok(Identity { device, inode, kind })
    }
    fn timestamp(&mut self) -> io::Result<Timestamp> {
        let seconds = i64::from_le_bytes(self.take(8)?.try_into().unwrap());
        let nanoseconds = self.u32()?;
        if nanoseconds >= 1_000_000_000 { return Err(invalid("invalid timestamp")); }
        Ok(Timestamp { seconds, nanoseconds })
    }
    fn path(&mut self) -> io::Result<PathBuf> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        if bytes.contains(&0) { return Err(invalid("NUL in record path")); }
        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
}

fn decode(bytes: &[u8]) -> io::Result<Record> {
    if bytes.len() > MAX_RECORD { return Err(invalid("record exceeds limit")); }
    let mut decoder = Decoder(bytes);
    if decoder.take(MAGIC.len())? != MAGIC { return Err(invalid("unknown record version")); }
    let source = decoder.path()?;
    let destination = decoder.path()?;
    let identity = decoder.identity()?;
    let published = decoder.identity()?;
    let size = decoder.u64()?;
    let mode = u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid mode"))?;
    let uid = u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid uid"))?;
    let gid = u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid gid"))?;
    let links = decoder.u64()?;
    let atime = decoder.timestamp()?;
    let mtime = decoder.timestamp()?;
    let ctime = decoder.timestamp()?;
    let output = match decoder.take(1)?[0] {
        0 => None,
        1 => Some(Observation { identity: decoder.identity()?, size: decoder.u64()?, mode: u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid output mode"))?, uid: u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid output uid"))?, gid: u32::try_from(decoder.u64()?).map_err(|_| invalid("invalid output gid"))?, links: decoder.u64()?, atime: decoder.timestamp()?, mtime: decoder.timestamp()?, ctime: decoder.timestamp()? }),
        _ => return Err(invalid("invalid output presence")),
    };
    let parent = match decoder.take(1)?[0] { 0 => None, 1 => Some(decoder.identity()?), _ => return Err(invalid("invalid parent presence")) };
    let anchor = match decoder.take(1)?[0] { 0 => None, 1 => Some(decoder.identity()?), _ => return Err(invalid("invalid anchor presence")) };
    let count = decoder.u32()? as usize;
    if count > decoder.0.len() / 17 { return Err(invalid("invalid ancestry length")); }
    let mut ancestry = Vec::with_capacity(count);
    for _ in 0..count { ancestry.push(decoder.identity()?); }
    let final_flags = decoder.u32()?;
    if !decoder.0.is_empty() { return Err(invalid("trailing record bytes")); }
    Ok(Record { source, destination, observed: Observation { identity, size, mode, uid, gid, links, atime, mtime, ctime }, published, output, parent, anchor, ancestry, final_flags })
}

pub(super) fn write_intent(parent: BorrowedFd<'_>, name: &std::ffi::OsStr, record: &Record) -> io::Result<Identity> {
    let fd = fs::openat(parent, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR)?;
    let identity = crate::roots::observe_fd(fd.as_fd())?.identity;
    fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
    let owner = rustix::process::geteuid().as_raw();
    let mut file = File::from(fd);
    let bytes = encode(record)?;
    file.write_all(MAGIC)?;
    file.write_all(&owner.to_le_bytes())?;
    let mut encoded_identity = Vec::new();
    put_identity(&mut encoded_identity, crate::roots::observe_fd(parent)?.identity);
    file.write_all(&encoded_identity)?;
    file.write_all(&(bytes.len() as u32).to_le_bytes())?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    crate::platform::sync_directory(parent)?;
    Ok(identity)
}


pub(super) struct IndexedRecords {
    records: Records,
    chains: Records,
    heads: Box<[u64]>,
    length: u64,
}

impl IndexedRecords {
    pub fn new(scratch: &Path) -> io::Result<Self> {
        Ok(Self { records: Records::new(scratch)?, chains: Records::new(scratch)?, heads: vec![u64::MAX; 65536].into_boxed_slice(), length: 0 })
    }

    fn bucket(identity: Identity) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        identity.hash(&mut hasher);
        hasher.finish() as usize & 65535
    }

    pub fn push(&mut self, record: &Record) -> io::Result<()> {
        let bucket = Self::bucket(record.observed.identity);
        let offset = self.records.end();
        self.records.push(record)?;
        self.chains.file.seek(SeekFrom::Start(self.length))?;
        self.chains.file.write_all(&self.heads[bucket].to_le_bytes())?;
        self.chains.file.write_all(&offset.to_le_bytes())?;
        self.heads[bucket] = self.length;
        self.length += 16;
        Ok(())
    }

    pub fn find_identity(&mut self, identity: Identity) -> io::Result<Option<Record>> {
        let mut cursor = self.heads[Self::bucket(identity)];
        while cursor != u64::MAX {
            if cursor >= self.length || !cursor.is_multiple_of(16) { return Err(invalid("invalid indexed record offset")); }
            self.chains.file.seek(SeekFrom::Start(cursor))?;
            let mut bytes = [0; 16];
            self.chains.file.read_exact(&mut bytes)?;
            let previous = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let mut offset = u64::from_le_bytes(bytes[8..].try_into().unwrap());
            let record = self.records.next(&mut offset)?.ok_or_else(|| invalid("indexed record missing"))?;
            if record.observed.identity == identity { return Ok(Some(record)); }
            if previous != u64::MAX && previous >= cursor { return Err(invalid("indexed record chain does not decrease")); }
            cursor = previous;
        }
        Ok(None)
    }
}

pub(super) fn validate_intent(parent: BorrowedFd<'_>, name: &std::ffi::OsStr, expected: Identity) -> io::Result<Record> {
    let fd = fs::openat(parent, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?;
    let observed = crate::roots::observe_fd(fd.as_fd())?;
    let owner = rustix::process::geteuid().as_raw();
    if observed.identity != expected || observed.identity.kind != EntryKind::RegularFile || observed.uid != owner || observed.links != 1 || observed.mode & 0o077 != 0 || observed.size > (MAX_RECORD + 64) as u64 {
        return Err(invalid("intent identity, owner, mode, or length is invalid"));
    }
    let mut file = File::from(fd);
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC { return Err(invalid("unknown intent version")); }
    let mut owner_bytes = [0; 4];
    file.read_exact(&mut owner_bytes)?;
    if u32::from_le_bytes(owner_bytes) != owner { return Err(invalid("intent owner does not match record")); }
    let mut parent_bytes = [0; 17];
    file.read_exact(&mut parent_bytes)?;
    if Decoder(&parent_bytes).identity()? != crate::roots::observe_fd(parent)?.identity { return Err(invalid("intent parent identity changed")); }
    let length = read_length(&mut file)?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    let mut extra = [0; 1];
    if file.read(&mut extra)? != 0 { return Err(invalid("trailing intent bytes")); }
    let record = decode(&bytes)?;
    for path in [&record.source, &record.destination] {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') { return Err(invalid("intent final name is invalid")); }
    }
    if !super::anchored::unchanged(observed, crate::roots::observe_fd(file.as_fd())?) { return Err(invalid("intent changed while reading")); }
    Ok(record)
}

impl Records {
    pub fn set_final_flags(&mut self, path: &Path, flags: u32) -> io::Result<()> {
        let mut cursor = 0;
        while let Some(record) = self.next(&mut cursor)? {
            if record.source == path {
                self.file.seek(SeekFrom::Start(cursor - 8))?;
                self.file.write_all(&flags.to_le_bytes())?;
                return Ok(());
            }
        }
        Err(invalid("final metadata record not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Record {
        let identity = Identity { device: 2, inode: 8, kind: EntryKind::RegularFile };
        let time = Timestamp { seconds: -1, nanoseconds: 2 };
        Record { source: PathBuf::from(OsString::from_vec(b"bad\xff\n".to_vec())), destination: PathBuf::from("destination"), observed: Observation { identity, size: 7, mode: 0o100644, uid: 1, gid: 2, links: 3, atime: time, mtime: time, ctime: time }, published: identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 }
    }
    #[test]
    fn malformed_unknown_record_rejected() {
        let encoded = encode(&sample()).unwrap();
        for length in 0..encoded.len() { assert!(decode(&encoded[..length]).is_err()); }
        let mut unknown = encoded.clone(); unknown[7] = 2; assert!(decode(&unknown).is_err());
        let mut trailing = encoded; trailing.push(0); assert!(decode(&trailing).is_err());
    }
    #[test]
    fn bounded_record_decode() { assert!(decode(&vec![0; MAX_RECORD + 1]).is_err()); }
    #[test]
    fn raw_byte_record_roundtrip() { let record = sample(); let decoded = decode(&encode(&record).unwrap()).unwrap(); assert_eq!(record.source, decoded.source); assert_eq!(record.observed, decoded.observed); }
}
