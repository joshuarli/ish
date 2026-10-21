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
const MAGIC: &[u8; 8] = b"ishfs\0\0\x02";
const OUTPUT_SLOT: usize = 94;
const NO_CHILD: u64 = u64::MAX;
const TREE_NODE_BYTES: u64 = 17;

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

#[derive(Clone, Copy)]
struct TreeNode {
    left: u64,
    right: u64,
    height: u8,
}

pub(super) struct Records {
    file: File,
    length: u64,
    record_count: u64,
    root: u64,
    indexed: bool,
    #[cfg(test)]
    lookup_work: u64,
}

impl Records {
    pub fn new(scratch: &Path) -> io::Result<Self> {
        Ok(Self { file: new_spool(scratch, "records")?, length: 0, record_count: 0, root: NO_CHILD, indexed: false, #[cfg(test)] lookup_work: 0 })
    }

    pub fn push(&mut self, record: &Record) -> io::Result<()> {
        let bytes = encode(record)?;
        let body_length = u64::try_from(bytes.len()).map_err(|_| invalid("record exceeds limit"))?;
        let end = self.length.checked_add(body_length).and_then(|length| length.checked_add(8 + TREE_NODE_BYTES)).ok_or_else(|| invalid("record spool size overflow"))?;
        let record_count = self.record_count.checked_add(1).ok_or_else(|| invalid("record count overflow"))?;
        self.file.seek(SeekFrom::Start(self.length))?;
        let length = u32::try_from(bytes.len()).map_err(|_| invalid("record exceeds limit"))?;
        self.file.write_all(&length.to_le_bytes())?;
        self.write_node(self.length, TreeNode { left: NO_CHILD, right: NO_CHILD, height: 1 })?;
        self.file.write_all(&bytes)?;
        self.file.write_all(&length.to_le_bytes())?;
        let offset = self.length;
        self.length = end;
        self.record_count = record_count;
        if self.indexed && let Err(error) = self.insert_path(offset, &record.source) {
            self.indexed = false;
            self.root = NO_CHILD;
            return Err(error);
        }
        Ok(())
    }

    pub fn next(&mut self, cursor: &mut u64) -> io::Result<Option<Record>> {
        if *cursor == self.length { return Ok(None); }
        if *cursor > self.length { return Err(invalid("record cursor exceeds spool")); }
        self.file.seek(SeekFrom::Start(*cursor))?;
        let length = read_length(&mut self.file)?;
        if u64::try_from(length).unwrap() + 8 + TREE_NODE_BYTES > self.length - *cursor { return Err(invalid("truncated record")); }
        let mut node_bytes = [0; TREE_NODE_BYTES as usize];
        self.file.read_exact(&mut node_bytes)?;
        let node = TreeNode { left: u64::from_le_bytes(node_bytes[..8].try_into().unwrap()), right: u64::from_le_bytes(node_bytes[8..16].try_into().unwrap()), height: node_bytes[16] };
        if node.height == 0 || node.height > 128 || [node.left, node.right].into_iter().any(|child| child != NO_CHILD && child >= self.length) { return Err(invalid("invalid record path tree node")); }
        let mut bytes = vec![0; length];
        self.file.read_exact(&mut bytes)?;
        if read_length(&mut self.file)? != length { return Err(invalid("record length mismatch")); }
        *cursor += length as u64 + 8 + TREE_NODE_BYTES;
        decode(&bytes).map(Some)
    }

    pub fn previous(&mut self, cursor: &mut u64) -> io::Result<Option<Record>> {
        if *cursor == 0 { return Ok(None); }
        if *cursor < 8 || *cursor > self.length { return Err(invalid("invalid reverse cursor")); }
        self.file.seek(SeekFrom::Start(*cursor - 4))?;
        let length = read_length(&mut self.file)?;
        let size = length as u64 + 8 + TREE_NODE_BYTES;
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
        Ok(self.find_entry(path)?.map(|(_, _, record)| record))
    }

    #[cfg(test)]
    pub(super) fn lookup_work(&self) -> u64 { self.lookup_work }

    fn find_entry(&mut self, path: &Path) -> io::Result<Option<(u64, u64, Record)>> {
        self.ensure_path_index()?;
        let mut current = self.root;
        let mut found = None;
        let mut visited = 0;
        while current != NO_CHILD {
            if visited >= self.record_count { return Err(invalid("record path tree contains a cycle")); }
            let node = self.read_node(current)?;
            let mut end = current;
            let record = self.next(&mut end)?.ok_or_else(|| invalid("indexed record missing"))?;
            #[cfg(test)]
            { self.lookup_work += 1; }
            match record.source.as_path().cmp(path) {
                std::cmp::Ordering::Less => current = node.right,
                std::cmp::Ordering::Equal => {
                    found = Some((current, end, record));
                    current = node.left;
                },
                std::cmp::Ordering::Greater => current = node.left,
            }
            visited += 1;
        }
        Ok(found)
    }

    fn ensure_path_index(&mut self) -> io::Result<()> {
        if self.indexed { return Ok(()); }
        self.root = NO_CHILD;
        let mut cursor = 0;
        while cursor < self.length {
            let offset = cursor;
            let record = self.next(&mut cursor)?.ok_or_else(|| invalid("record missing while indexing"))?;
            #[cfg(test)]
            { self.lookup_work += 1; }
            self.write_node(offset, TreeNode { left: NO_CHILD, right: NO_CHILD, height: 1 })?;
            self.insert_path(offset, &record.source)?;
        }
        if cursor != self.length { return Err(invalid("record index ended between records")); }
        self.indexed = true;
        Ok(())
    }

    fn insert_path(&mut self, offset: u64, path: &Path) -> io::Result<()> {
        if self.root == NO_CHILD {
            self.root = offset;
            return Ok(());
        }
        let mut ancestors = [(NO_CHILD, false); 128];
        let mut depth = 0;
        let mut current = self.root;
        loop {
            if depth == ancestors.len() { return Err(invalid("record path tree height exceeds limit")); }
            let node = self.read_node(current)?;
            let mut cursor = current;
            let record = self.next(&mut cursor)?.ok_or_else(|| invalid("indexed record missing"))?;
            #[cfg(test)]
            { self.lookup_work += 1; }
            let right = record.source.as_path() <= path;
            ancestors[depth] = (current, right);
            depth += 1;
            let child = if right { node.right } else { node.left };
            if child == NO_CHILD {
                let mut updated = node;
                if right { updated.right = offset; } else { updated.left = offset; }
                self.write_node(current, updated)?;
                break;
            }
            current = child;
        }
        for level in (0..depth).rev() {
            let subtree = self.rebalance(ancestors[level].0)?;
            if level == 0 {
                self.root = subtree;
            } else {
                let (parent, right) = ancestors[level - 1];
                let mut node = self.read_node(parent)?;
                if right { node.right = subtree; } else { node.left = subtree; }
                self.write_node(parent, node)?;
            }
        }
        Ok(())
    }

    fn read_node(&mut self, offset: u64) -> io::Result<TreeNode> {
        if offset >= self.length { return Err(invalid("record tree offset exceeds spool")); }
        self.file.seek(SeekFrom::Start(offset.checked_add(4).ok_or_else(|| invalid("record tree offset overflow"))?))?;
        let mut bytes = [0; TREE_NODE_BYTES as usize];
        self.file.read_exact(&mut bytes)?;
        let node = TreeNode { left: u64::from_le_bytes(bytes[..8].try_into().unwrap()), right: u64::from_le_bytes(bytes[8..16].try_into().unwrap()), height: bytes[16] };
        if node.height == 0 || node.height > 128 || [node.left, node.right].into_iter().any(|child| child != NO_CHILD && child >= self.length) { return Err(invalid("invalid record path tree node")); }
        Ok(node)
    }

    fn write_node(&mut self, offset: u64, node: TreeNode) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset.checked_add(4).ok_or_else(|| invalid("record tree offset overflow"))?))?;
        self.file.write_all(&node.left.to_le_bytes())?;
        self.file.write_all(&node.right.to_le_bytes())?;
        self.file.write_all(&[node.height])
    }

    fn node_height(&mut self, offset: u64) -> io::Result<u8> {
        if offset == NO_CHILD { Ok(0) } else { Ok(self.read_node(offset)?.height) }
    }

    fn update_height(&mut self, offset: u64) -> io::Result<TreeNode> {
        let mut node = self.read_node(offset)?;
        let height = self.node_height(node.left)?.max(self.node_height(node.right)?).checked_add(1).ok_or_else(|| invalid("record path tree height overflow"))?;
        node.height = height;
        self.write_node(offset, node)?;
        Ok(node)
    }

    fn balance(&mut self, node: TreeNode) -> io::Result<i16> {
        Ok(i16::from(self.node_height(node.left)?) - i16::from(self.node_height(node.right)?))
    }

    fn rebalance(&mut self, offset: u64) -> io::Result<u64> {
        let node = self.update_height(offset)?;
        let balance = self.balance(node)?;
        if balance > 1 {
            let left = self.read_node(node.left)?;
            if self.balance(left)? < 0 {
                let new_left = self.rotate_left(node.left)?;
                let mut updated = self.read_node(offset)?;
                updated.left = new_left;
                self.write_node(offset, updated)?;
            }
            return self.rotate_right(offset);
        }
        if balance < -1 {
            let right = self.read_node(node.right)?;
            if self.balance(right)? > 0 {
                let new_right = self.rotate_right(node.right)?;
                let mut updated = self.read_node(offset)?;
                updated.right = new_right;
                self.write_node(offset, updated)?;
            }
            return self.rotate_left(offset);
        }
        Ok(offset)
    }

    fn rotate_left(&mut self, offset: u64) -> io::Result<u64> {
        let mut root = self.read_node(offset)?;
        let pivot_offset = root.right;
        if pivot_offset == NO_CHILD { return Err(invalid("invalid left rotation")); }
        let mut pivot = self.read_node(pivot_offset)?;
        root.right = pivot.left;
        self.write_node(offset, root)?;
        self.update_height(offset)?;
        pivot.left = offset;
        self.write_node(pivot_offset, pivot)?;
        self.update_height(pivot_offset)?;
        Ok(pivot_offset)
    }

    fn rotate_right(&mut self, offset: u64) -> io::Result<u64> {
        let mut root = self.read_node(offset)?;
        let pivot_offset = root.left;
        if pivot_offset == NO_CHILD { return Err(invalid("invalid right rotation")); }
        let mut pivot = self.read_node(pivot_offset)?;
        root.left = pivot.right;
        self.write_node(offset, root)?;
        self.update_height(offset)?;
        pivot.right = offset;
        self.write_node(pivot_offset, pivot)?;
        self.update_height(pivot_offset)?;
        Ok(pivot_offset)
    }

    pub fn set_output(&mut self, path: &Path, output: Observation) -> io::Result<()> {
        let (offset, _, record) = self.find_entry(path)?.ok_or_else(|| invalid("output record not found"))?;
        if record.source != path { return Err(invalid("output record path changed")); }
        self.file.seek(SeekFrom::Start(offset + output_field_offset(&record)?))?;
        self.file.write_all(&encode_output(Some(output)))?;
        Ok(())
    }

}

fn new_spool(scratch: &Path, purpose: &str) -> io::Result<File> {
    let directory = fs::open(scratch, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let name = unique_name(purpose);
    let fd = fs::openat(&directory, &name, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR)?;
    fs::unlinkat(&directory, &name, AtFlags::empty())?;
    Ok(fd.into())
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

fn put_observation_fields(bytes: &mut Vec<u8>, observation: Observation) {
    for value in [observation.size, observation.mode as u64, observation.uid as u64, observation.gid as u64, observation.links] { bytes.extend_from_slice(&value.to_le_bytes()); }
    for value in [observation.atime, observation.mtime, observation.ctime] {
        bytes.extend_from_slice(&value.seconds.to_le_bytes());
        bytes.extend_from_slice(&value.nanoseconds.to_le_bytes());
    }
}

fn put_observation(bytes: &mut Vec<u8>, observation: Observation) {
    put_identity(bytes, observation.identity);
    put_observation_fields(bytes, observation);
}

fn put_path(bytes: &mut Vec<u8>, path: &Path) -> io::Result<()> {
    let path = path.as_os_str().as_bytes();
    let length = u32::try_from(path.len()).map_err(|_| invalid("record path exceeds limit"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(path);
    Ok(())
}

fn encode_output(output: Option<Observation>) -> [u8; OUTPUT_SLOT] {
    let mut bytes = Vec::with_capacity(OUTPUT_SLOT);
    match output {
        Some(observation) => {
            bytes.push(1);
            put_observation(&mut bytes, observation);
        },
        None => bytes.push(0),
    }
    bytes.resize(OUTPUT_SLOT, 0);
    bytes.try_into().unwrap()
}

fn output_field_offset(record: &Record) -> io::Result<u64> {
    let source_len = u64::try_from(record.source.as_os_str().as_bytes().len()).map_err(|_| invalid("record path exceeds limit"))?;
    let destination_len = u64::try_from(record.destination.as_os_str().as_bytes().len()).map_err(|_| invalid("record path exceeds limit"))?;
    let path_fields = 8u64.checked_add(source_len).and_then(|value| value.checked_add(destination_len)).ok_or_else(|| invalid("record offset overflow"))?;
    let fixed_fields = u64::try_from(4 + TREE_NODE_BYTES as usize + MAGIC.len() + 34 + 40 + 36).unwrap();
    path_fields.checked_add(fixed_fields).ok_or_else(|| invalid("record offset overflow"))
}

fn encode(record: &Record) -> io::Result<Vec<u8>> {
    let mut bytes = MAGIC.to_vec();
    put_path(&mut bytes, &record.source)?;
    put_path(&mut bytes, &record.destination)?;
    put_identity(&mut bytes, record.observed.identity);
    put_identity(&mut bytes, record.published);
    put_observation_fields(&mut bytes, record.observed);
    bytes.extend_from_slice(&encode_output(record.output));
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
    fn observation(&mut self, output: bool) -> io::Result<Observation> {
        let identity = self.identity()?;
        self.observation_fields(identity, output)
    }
    fn observation_fields(&mut self, identity: Identity, output: bool) -> io::Result<Observation> {
        let size = self.u64()?;
        let mode = u32::try_from(self.u64()?).map_err(|_| invalid(if output { "invalid output mode" } else { "invalid mode" }))?;
        let uid = u32::try_from(self.u64()?).map_err(|_| invalid(if output { "invalid output uid" } else { "invalid uid" }))?;
        let gid = u32::try_from(self.u64()?).map_err(|_| invalid(if output { "invalid output gid" } else { "invalid gid" }))?;
        let links = self.u64()?;
        let atime = self.timestamp()?;
        let mtime = self.timestamp()?;
        let ctime = self.timestamp()?;
        Ok(Observation { identity, size, mode, uid, gid, links, atime, mtime, ctime })
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
    let observed = decoder.observation_fields(identity, false)?;
    let mut output_slot = Decoder(decoder.take(OUTPUT_SLOT)?);
    let presence = output_slot.take(1)?[0];
    let output = match presence {
        0 => {
            if output_slot.0.iter().any(|byte| *byte != 0) { return Err(invalid("invalid empty output slot")); }
            None
        },
        1 => Some(output_slot.observation(true)?),
        _ => return Err(invalid("invalid output presence")),
    };
    if !output_slot.0.is_empty() && presence == 1 { return Err(invalid("invalid output slot length")); }
    let parent = match decoder.take(1)?[0] { 0 => None, 1 => Some(decoder.identity()?), _ => return Err(invalid("invalid parent presence")) };
    let anchor = match decoder.take(1)?[0] { 0 => None, 1 => Some(decoder.identity()?), _ => return Err(invalid("invalid anchor presence")) };
    let count = decoder.u32()? as usize;
    if count > decoder.0.len() / 17 { return Err(invalid("invalid ancestry length")); }
    let mut ancestry = Vec::with_capacity(count);
    for _ in 0..count { ancestry.push(decoder.identity()?); }
    let final_flags = decoder.u32()?;
    if !decoder.0.is_empty() { return Err(invalid("trailing record bytes")); }
    Ok(Record { source, destination, observed, published, output, parent, anchor, ancestry, final_flags })
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
        let (_, end, _) = self.find_entry(path)?.ok_or_else(|| invalid("final metadata record not found"))?;
        self.file.seek(SeekFrom::Start(end - 8))?;
        self.file.write_all(&flags.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(unique_name("record-test"));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
    }
    fn sample() -> Record {
        let identity = Identity { device: 2, inode: 8, kind: EntryKind::RegularFile };
        let time = Timestamp { seconds: -1, nanoseconds: 2 };
        Record { source: PathBuf::from(OsString::from_vec(b"bad\xff\n".to_vec())), destination: PathBuf::from("destination"), observed: Observation { identity, size: 7, mode: 0o100644, uid: 1, gid: 2, links: 3, atime: time, mtime: time, ctime: time }, published: identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 }
    }
    #[test]
    fn malformed_unknown_record_rejected() {
        let encoded = encode(&sample()).unwrap();
        for length in 0..encoded.len() { assert!(decode(&encoded[..length]).is_err()); }
        let mut old_version = encoded.clone(); old_version[7] = 1; assert!(decode(&old_version).is_err());
        let mut unknown = encoded.clone(); unknown[7] = 3; assert!(decode(&unknown).is_err());
        let mut trailing = encoded; trailing.push(0); assert!(decode(&trailing).is_err());
    }
    #[test]
    fn bounded_record_decode() { assert!(decode(&vec![0; MAX_RECORD + 1]).is_err()); }
    #[test]
    fn raw_byte_record_roundtrip() { let record = sample(); let decoded = decode(&encode(&record).unwrap()).unwrap(); assert_eq!(record.source, decoded.source); assert_eq!(record.observed, decoded.observed); }

    #[test]
    fn repeated_find_work_grows_with_n_log_n() {
        let directory = Scratch::new();
        let sizes = [128usize, 256, 512, 1024];
        let mut previous_work = 0;
        for size in sizes {
            let mut records = Records::new(&directory.0).unwrap();
            let mut paths = Vec::with_capacity(size);
            for index in 0..size {
                let path = PathBuf::from(format!("entry-{index:08}"));
                let mut record = sample();
                record.source = path.clone();
                records.push(&record).unwrap();
                paths.push(path);
            }
            for path in &paths { assert!(records.find(path).unwrap().is_some()); }
            let work = records.lookup_work();
            let logarithm = usize::BITS - (size - 1).leading_zeros();
            assert!(work <= (size * logarithm as usize * 5) as u64);
            if previous_work != 0 { assert!(work * 4 <= previous_work * 10); }
            eprintln!("size={size} record_visits={work}");
            previous_work = work;
        }
    }

    #[test]
    fn indexed_mutations_preserve_first_duplicate_and_spool_length() {
        let directory = Scratch::new();
        let mut records = Records::new(&directory.0).unwrap();
        let mut first = sample();
        first.source = PathBuf::from("duplicate");
        first.observed.size = 11;
        records.push(&first).unwrap();
        let mut second = first.clone();
        second.observed.size = 22;
        records.push(&second).unwrap();
        let end = records.end();
        let output = Observation { identity: Identity { device: 4, inode: 9, kind: EntryKind::RegularFile }, size: 30, mode: 0o100600, uid: 5, gid: 6, links: 1, atime: Timestamp { seconds: 3, nanoseconds: 4 }, mtime: Timestamp { seconds: 5, nanoseconds: 6 }, ctime: Timestamp { seconds: 7, nanoseconds: 8 } };
        records.set_output(Path::new("duplicate"), output).unwrap();
        records.set_final_flags(Path::new("duplicate"), 0x55aa).unwrap();
        assert_eq!(records.end(), end);
        let first = records.find(Path::new("duplicate")).unwrap().unwrap();
        assert_eq!(first.observed.size, 11);
        assert_eq!(first.output, Some(output));
        assert_eq!(first.final_flags, 0x55aa);
        let mut cursor = 0;
        assert_eq!(records.next(&mut cursor).unwrap().unwrap().output, Some(output));
        assert_eq!(records.next(&mut cursor).unwrap().unwrap().output, None);
        let mut reverse = records.end();
        assert_eq!(records.previous(&mut reverse).unwrap().unwrap().observed.size, 22);
        assert_eq!(records.previous(&mut reverse).unwrap().unwrap().observed.size, 11);
        let mut appended = sample();
        appended.source = PathBuf::from("later");
        records.push(&appended).unwrap();
        assert_eq!(records.find(Path::new("later")).unwrap(), Some(appended));
    }

    #[test]
    fn indexed_lookup_preserves_path_equality_and_raw_spelling() {
        let directory = Scratch::new();
        let mut records = Records::new(&directory.0).unwrap();
        let mut record = sample();
        record.source = PathBuf::from("tree/");
        records.push(&record).unwrap();
        let found = records.find(Path::new("tree")).unwrap().unwrap();
        assert_eq!(found.source.as_os_str().as_bytes(), b"tree/");
    }

    #[test]
    fn malformed_v2_empty_output_rejected() {
        let record = sample();
        let mut encoded = encode(&record).unwrap();
        let output_start = (output_field_offset(&record).unwrap() - 4 - TREE_NODE_BYTES) as usize;
        encoded[output_start + 1] = 1;
        assert!(decode(&encoded).is_err());
    }

    #[test]
    fn wide_deep_and_skipped_merge_lookups_remain_bounded() {
        let directory = Scratch::new();
        let mut directories = Records::new(&directory.0).unwrap();
        let mut skipped = Records::new(&directory.0).unwrap();
        let mut directory_paths = Vec::new();
        let mut skipped_paths = Vec::new();
        for index in 0..256 {
            let path = PathBuf::from(format!("merge/wide-{index:04}/child"));
            let mut record = sample();
            record.source = path.clone();
            directories.push(&record).unwrap();
            directory_paths.push(path);
            let skip_path = PathBuf::from(format!("merge/wide-{index:04}/retained"));
            let mut skipped_record = sample();
            skipped_record.source = skip_path.clone();
            skipped.push(&skipped_record).unwrap();
            skipped_paths.push(skip_path);
        }
        let mut deep = PathBuf::from("merge/deep");
        for index in 0..64 {
            deep.push(format!("level-{index:02}"));
            let mut record = sample();
            record.source = deep.clone();
            directories.push(&record).unwrap();
            directory_paths.push(deep.clone());
        }
        for path in &directory_paths { assert_eq!(directories.find(path).unwrap().unwrap().source, *path); }
        for path in &skipped_paths {
            assert!(directories.find(path).unwrap().is_none());
            assert_eq!(skipped.find(path).unwrap().unwrap().source, *path);
        }
        let total_records = directory_paths.len() + skipped_paths.len();
        let total_lookups = directory_paths.len() + skipped_paths.len() * 2;
        let logarithm = usize::BITS - total_records.leading_zeros();
        let work = directories.lookup_work() + skipped.lookup_work();
        assert!(work <= ((total_records + total_lookups) * logarithm as usize * 4) as u64);
        eprintln!("wide_deep_skipped records={total_records} lookups={total_lookups} record_visits={work}");
    }
}
