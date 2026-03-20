use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

const CACHE_MAGIC_V1: &[u8; 4] = b"ISH\x01";
const CACHE_MAGIC_V2: &[u8; 4] = b"ISH\x02";
const CACHE_MAGIC_V3: &[u8; 4] = b"ISH\x03";

/// v1/v2 header: magic(4) + reserved(8) + entry_count(4) + arena_size(4)
const V2_HEADER_SIZE: usize = 20;
/// v3 header: magic(4) + entry_count(4) + arena_size(4)
const V3_HEADER_SIZE: usize = 12;

/// 1998-01-01T00:00:00 UTC as Unix epoch seconds.
/// v3 timestamps are stored relative to this, extending range to ~2134.
const TS_EPOCH: u32 = 883_612_800;

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Current epoch seconds via libc::time (commpage on macOS, vDSO on Linux — no syscall).
fn now_secs() -> u32 {
    unsafe { libc::time(std::ptr::null_mut()) as u32 }
}

pub struct History {
    /// All entry text packed into a single allocation.
    arena: String,
    /// (start, len) byte offsets into `arena` for each entry.
    offsets: Vec<(u32, u16)>,
    /// Epoch seconds when each entry was last used. Parallel to `offsets`.
    timestamps: Vec<u32>,
    /// Hash index for O(1) duplicate checks in add().
    hashes: HashSet<u64>,
    path: PathBuf,
    /// Byte offset into the text file we've read up to. Enables incremental
    /// sync — only new bytes appended by other shells are read.
    file_pos: u64,
    /// Per-entry flag: true if the entry was loaded at startup or added by
    /// this shell session (`add()`). Up-arrow navigation uses only local
    /// entries; Ctrl+R and autosuggestion search everything.
    local: Vec<bool>,
    /// Set when the cache was corrupt at load time. Prevents overwriting the
    /// (possibly recoverable) cache file until the user resolves it.
    cache_dirty: bool,
}

impl History {
    pub fn load() -> Self {
        let path = history_path();

        match Self::load_from_cache(&path) {
            Ok(Some(mut hist)) => {
                // Cache loaded — sync any new entries from the text file
                hist.sync();
                hist
            }
            Ok(None) => {
                // No cache file (first launch) — build from text, write cache
                let mut hist = Self::load_from_text(&path);
                hist.file_pos = fs::metadata(&hist.path).map(|m| m.len()).unwrap_or(0);
                if !hist.offsets.is_empty() {
                    hist.save_cache();
                }
                hist
            }
            Err(()) => {
                // Cache corrupt — load text file but do NOT write cache or
                // truncate the text file. The corrupt cache is left for the
                // user to inspect/delete manually.
                let mut hist = Self::load_from_text(&path);
                hist.file_pos = fs::metadata(&hist.path).map(|m| m.len()).unwrap_or(0);
                hist.cache_dirty = true;
                hist
            }
        }
    }

    /// Rebuild the binary cache from the text history file, replacing the
    /// current in-memory state and overwriting any existing cache on disk.
    pub fn rebuild(&mut self) {
        let mut fresh = Self::load_from_text(&self.path);
        fresh.file_pos = fs::metadata(&fresh.path).map(|m| m.len()).unwrap_or(0);
        fresh.cache_dirty = false;
        fresh.save_cache();
        eprintln!(
            "ish: rebuilt history cache — {} entries",
            fresh.offsets.len()
        );
        *self = fresh;
    }

    fn load_from_text(path: &Path) -> Self {
        let (arena, offsets, hashes) = match fs::read(path) {
            Ok(data) => {
                let line_count = memchr_count(b'\n', &data);
                let mut seen = HashSet::with_capacity(line_count);
                let mut deduped: Vec<&str> = Vec::with_capacity(line_count);

                for chunk in data.rsplit(|&b| b == b'\n') {
                    if let Ok(line) = std::str::from_utf8(chunk)
                        && !line.is_empty()
                    {
                        let h = hash_str(line);
                        if seen.insert(h) {
                            deduped.push(line);
                        }
                    }
                }
                deduped.reverse();

                let total: usize = deduped.iter().map(|s| s.len()).sum();
                let mut arena = String::with_capacity(total);
                let mut offsets = Vec::with_capacity(deduped.len());
                for line in &deduped {
                    let start = arena.len() as u32;
                    arena.push_str(line);
                    offsets.push((start, line.len() as u16));
                }

                (arena, offsets, seen)
            }
            Err(_) => (String::new(), Vec::new(), HashSet::new()),
        };

        // No timestamps in text format — use current time for all entries
        let ts = now_secs();
        let count = offsets.len();
        let timestamps = vec![ts; count];

        Self {
            arena,
            offsets,
            timestamps,
            hashes,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![true; count],
            cache_dirty: false,
        }
    }

    /// Returns `Ok(Some)` on success, `Ok(None)` if no cache file exists,
    /// `Err(())` if the cache exists but is corrupt or unreadable.
    fn load_from_cache(path: &Path) -> Result<Option<Self>, ()> {
        let data = match fs::read(cache_path()) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                eprintln!("ish: history cache unreadable: {e}");
                return Err(());
            }
        };

        match Self::parse_cache(&data, path) {
            Some(hist) => Ok(Some(hist)),
            None => {
                eprintln!(
                    "ish: history cache corrupt ({} bytes) — loading text file only",
                    data.len()
                );
                Err(())
            }
        }
    }

    fn parse_cache(data: &[u8], path: &Path) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }

        match &data[0..4] {
            x if x == CACHE_MAGIC_V3 => Self::parse_v3(data, path),
            x if x == CACHE_MAGIC_V2 => Self::parse_v1v2(data, path, 2),
            x if x == CACHE_MAGIC_V1 => Self::parse_v1v2(data, path, 1),
            _ => None,
        }
    }

    /// Parse v3 format: [magic(4)][entry_count(4)][arena_size(4)][timestamps: N×4][arena: \0-delimited]
    fn parse_v3(data: &[u8], path: &Path) -> Option<Self> {
        if data.len() < V3_HEADER_SIZE {
            return None;
        }

        let entry_count = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
        let arena_size = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;

        let expected = V3_HEADER_SIZE + entry_count * 4 + arena_size;
        if data.len() != expected {
            return None;
        }

        // Bulk-copy timestamps, converting from 1998-epoch to Unix epoch
        let ts_start = V3_HEADER_SIZE;
        let mut timestamps = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let off = ts_start + i * 4;
            let stored = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
            timestamps.push(stored.wrapping_add(TS_EPOCH));
        }

        // Arena: null-delimited entries
        let arena_start = ts_start + entry_count * 4;
        let arena_bytes = &data[arena_start..arena_start + arena_size];
        let arena_str = std::str::from_utf8(arena_bytes).ok()?;

        let mut arena = String::with_capacity(arena_size);
        let mut offsets = Vec::with_capacity(entry_count);
        let mut hashes = HashSet::with_capacity(entry_count);
        let mut count = 0;

        for entry in arena_str.split('\0') {
            if entry.is_empty() {
                continue;
            }
            let start = arena.len() as u32;
            arena.push_str(entry);
            offsets.push((start, entry.len() as u16));
            hashes.insert(hash_str(entry));
            count += 1;
        }

        if count != entry_count {
            return None;
        }

        Some(Self {
            arena,
            offsets,
            timestamps,
            hashes,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![true; count],
            cache_dirty: false,
        })
    }

    /// Parse legacy v1/v2 format for migration.
    fn parse_v1v2(data: &[u8], path: &Path, version: u8) -> Option<Self> {
        if data.len() < V2_HEADER_SIZE {
            return None;
        }

        // Skip reserved field (bytes 4..12)
        let entry_count = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
        let arena_size = u32::from_le_bytes(data[16..20].try_into().ok()?) as usize;

        let timestamps_size = if version >= 2 { entry_count * 4 } else { 0 };
        let expected =
            V2_HEADER_SIZE + entry_count * 8 + timestamps_size + entry_count * 6 + arena_size;
        if data.len() != expected {
            return None;
        }

        let mut pos = V2_HEADER_SIZE;

        // Skip hashes (no longer stored in-memory)
        let mut hashes = HashSet::with_capacity(entry_count);
        pos += entry_count * 8;

        // Read timestamps (v2+) or default to 0 (v1)
        let timestamps = if version >= 2 {
            let mut ts = Vec::with_capacity(entry_count);
            for _ in 0..entry_count {
                ts.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
                pos += 4;
            }
            ts
        } else {
            vec![0; entry_count]
        };

        // Read offsets
        let mut offsets = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let start = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
            let len = u16::from_le_bytes(data[pos + 4..pos + 6].try_into().ok()?);
            offsets.push((start, len));
            pos += 6;
        }

        // Read arena
        let arena = String::from_utf8(data[pos..pos + arena_size].to_vec()).ok()?;

        // Validate offsets and build hash index
        for &(start, len) in &offsets {
            let s = start as usize;
            let l = len as usize;
            if s + l > arena.len() {
                return None;
            }
            hashes.insert(hash_str(&arena[s..s + l]));
        }

        let count = offsets.len();
        Some(Self {
            arena,
            offsets,
            timestamps,
            hashes,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![true; count],
            cache_dirty: false,
        })
    }

    /// Read new entries appended to the text file by other shell instances.
    /// One stat() call to check for growth; reads only the new tail bytes.
    /// Called at each prompt and before Ctrl+R history search.
    pub fn sync(&mut self) {
        let file_size = match fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        if file_size == self.file_pos {
            return; // fast path: nothing new
        }

        if file_size < self.file_pos {
            // File was truncated (compacted by another shell). Our in-memory
            // entries are still valid — just reset the read position.
            self.file_pos = file_size;
            return;
        }

        // Read only the new tail
        let mut f = match fs::File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };

        use std::io::{Read, Seek, SeekFrom};
        if f.seek(SeekFrom::Start(self.file_pos)).is_err() {
            return;
        }

        let mut tail = String::new();
        if f.read_to_string(&mut tail).is_err() {
            return;
        }

        let ts = now_secs();
        for line in tail.lines() {
            if line.is_empty() {
                continue;
            }
            let h = hash_str(line);
            if self.hashes.contains(&h) {
                // If a local entry already has this command, skip —
                // don't disturb the session's up-arrow timeline.
                let any_local =
                    self.offsets
                        .iter()
                        .zip(self.local.iter())
                        .any(|(&(start, len), &is_local)| {
                            is_local
                                && &self.arena[start as usize..start as usize + len as usize]
                                    == line
                        });
                if any_local {
                    continue;
                }
                // Remove old non-local duplicate, re-add at end
                let mut i = 0;
                while i < self.offsets.len() {
                    let (start, len) = self.offsets[i];
                    if &self.arena[start as usize..start as usize + len as usize] == line {
                        self.offsets.remove(i);
                        self.timestamps.remove(i);
                        self.local.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            let start = self.arena.len() as u32;
            self.arena.push_str(line);
            self.offsets.push((start, line.len() as u16));
            self.timestamps.push(ts);
            self.local.push(false);
            self.hashes.insert(h);
        }

        self.file_pos = file_size;
    }

    /// Write v3 binary cache, then truncate text file.
    /// v3 format: [magic(4)][entry_count(4)][arena_size(4)][timestamps: N×4][arena: \0-delimited]
    /// Atomic: writes cache to .tmp then renames.
    pub fn save_cache(&self) {
        if self.cache_dirty {
            return;
        }
        let cache = cache_path();
        let tmp = cache.with_extension("bin.tmp");

        let entry_count = self.offsets.len();

        // Build null-delimited arena
        let mut arena_buf = Vec::new();
        for &(start, len) in &self.offsets {
            arena_buf.extend_from_slice(
                &self.arena.as_bytes()[start as usize..start as usize + len as usize],
            );
            arena_buf.push(0);
        }
        let arena_size = arena_buf.len();

        let total = V3_HEADER_SIZE + entry_count * 4 + arena_size;
        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(CACHE_MAGIC_V3);
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(arena_size as u32).to_le_bytes());

        // Timestamps (offset from 1998 epoch)
        for &ts in &self.timestamps {
            buf.extend_from_slice(&ts.wrapping_sub(TS_EPOCH).to_le_bytes());
        }

        // Null-delimited arena
        buf.extend_from_slice(&arena_buf);

        // Guard: refuse to overwrite a larger cache with a much smaller one.
        if let Ok(existing) = fs::read(&cache)
            && existing.len() >= 4
        {
            let old_count = match &existing[0..4] {
                x if x == CACHE_MAGIC_V3 && existing.len() >= V3_HEADER_SIZE => {
                    u32::from_le_bytes(existing[4..8].try_into().unwrap_or_default()) as usize
                }
                _ if existing.len() >= V2_HEADER_SIZE => {
                    u32::from_le_bytes(existing[12..16].try_into().unwrap_or_default()) as usize
                }
                _ => 0,
            };
            if entry_count < old_count / 2 && old_count > 100 {
                eprintln!(
                    "ish: refusing to shrink history cache from {old_count} to {entry_count} entries"
                );
                let _ = fs::remove_file(&tmp);
                return;
            }
        }

        if fs::write(&tmp, &buf).is_ok() && fs::rename(&tmp, &cache).is_ok() {
            // Cache written — truncate text file since its contents
            // are now in the cache. New commands append to a fresh file.
            let _ = fs::File::create(&self.path);
        }
    }

    /// Merge text tail into cache, dedup, rewrite cache + truncate text.
    /// Uses flock to serialize across concurrent shells.
    pub fn compact(&mut self) {
        let lock = lock_path();
        if let Some(parent) = lock.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let lock_fd = match fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&lock)
        {
            Ok(f) => f,
            Err(_) => {
                self.file_pos = 0;
                self.sync();
                self.save_cache();
                return;
            }
        };

        use std::os::fd::AsRawFd;
        // Try non-blocking lock — skip if another shell is compacting
        let rc = unsafe { libc::flock(lock_fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            self.file_pos = 0;
            self.sync();
            self.save_cache();
            return;
        }

        // Re-read text tail from other shells, merge into our state
        self.file_pos = 0;
        self.sync();

        // save_cache writes cache + truncates text file
        self.save_cache();
        // lock released when lock_fd drops
    }

    /// Create from pre-existing entries (for testing/benchmarks).
    pub fn from_entries(entries: Vec<String>) -> Self {
        let ts = now_secs();
        let total: usize = entries.iter().map(|e| e.len()).sum();
        let mut arena = String::with_capacity(total);
        let mut offsets = Vec::with_capacity(entries.len());
        let mut timestamps = Vec::with_capacity(entries.len());
        let mut hashes = HashSet::with_capacity(entries.len());
        for e in &entries {
            let start = arena.len() as u32;
            let h = hash_str(e);
            arena.push_str(e);
            offsets.push((start, e.len() as u16));
            timestamps.push(ts);
            hashes.insert(h);
        }
        let count = offsets.len();
        Self {
            arena,
            offsets,
            timestamps,
            hashes,
            path: PathBuf::from("/dev/null"),
            file_pos: 0,
            local: vec![true; count],
            cache_dirty: false,
        }
    }

    /// Add entry. Deduplicates (removes prior occurrence).
    pub fn add(&mut self, line: &str) {
        // Collapse newlines to spaces to prevent history file corruption.
        let line = line.trim().replace('\n', " ");
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        let h = hash_str(line);
        if self.hashes.contains(&h) {
            // Likely duplicate — remove prior occurrence from all parallel vecs
            let mut i = 0;
            while i < self.offsets.len() {
                let (start, len) = self.offsets[i];
                if &self.arena[start as usize..start as usize + len as usize] == line {
                    self.offsets.remove(i);
                    self.timestamps.remove(i);
                    self.local.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        // Truncate entries that exceed u16 max (64KB) — shouldn't happen in practice
        let len = line.len().min(u16::MAX as usize);
        let start = self.arena.len() as u32;
        self.arena.push_str(&line[..len]);
        self.offsets.push((start, len as u16));
        self.timestamps.push(now_secs());
        self.local.push(true);
        self.hashes.insert(h);

        // Append to file
        self.append_to_file(line);
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Get the timestamp (epoch seconds) for entry at index.
    pub fn timestamp(&self, idx: usize) -> u32 {
        self.timestamps[idx]
    }

    /// Prefix search: find entries that start with `prefix`, starting from
    /// the end and skipping `skip` matches. Returns the entry text.
    pub fn prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.offsets
            .iter()
            .rev()
            .filter_map(|&(start, len)| {
                let s = &self.arena[start as usize..start as usize + len as usize];
                s.starts_with(prefix).then_some(s)
            })
            .nth(skip)
    }

    /// Get the `skip`'th local entry from the end (for up-arrow navigation).
    /// Local entries are those loaded at startup or added by this session.
    pub fn local_get(&self, skip: usize) -> Option<&str> {
        self.offsets
            .iter()
            .zip(self.local.iter())
            .rev()
            .filter(|&(_, &is_local)| is_local)
            .nth(skip)
            .map(|(&(start, len), _)| &self.arena[start as usize..start as usize + len as usize])
    }

    /// Prefix search over local entries only (for up-arrow with partial input).
    pub fn local_prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.offsets
            .iter()
            .zip(self.local.iter())
            .rev()
            .filter(|&(_, &is_local)| is_local)
            .filter_map(|(&(start, len), _)| {
                let s = &self.arena[start as usize..start as usize + len as usize];
                s.starts_with(prefix).then_some(s)
            })
            .nth(skip)
    }

    /// Fuzzy (subsequence) search: every char of `query` appears in the entry
    /// in order, case-insensitive. Returns matching entries most-recent-first,
    /// with entry index and indices of matching chars.
    pub fn fuzzy_search(&self, query: &str) -> Vec<FuzzyMatch> {
        if query.is_empty() {
            return (0..self.offsets.len())
                .rev()
                .map(|idx| FuzzyMatch {
                    entry_idx: idx,
                    match_positions: [0; 32],
                    match_count: 0,
                })
                .collect();
        }

        let query_lower = lowercase_query(query);
        let mut results = Vec::new();

        for (idx, &(start, len)) in self.offsets.iter().enumerate().rev() {
            let entry = &self.arena[start as usize..start as usize + len as usize];
            if let Some((positions, count)) = subsequence_match(&query_lower, entry) {
                results.push(FuzzyMatch {
                    entry_idx: idx,
                    match_positions: positions,
                    match_count: count,
                });
            }
        }

        results
    }

    /// Like `fuzzy_search` but appends into a caller-owned Vec (zero-alloc reuse).
    /// Caps at `limit` results since the pager only shows a screenful.
    pub fn fuzzy_search_into(&self, query: &str, results: &mut Vec<FuzzyMatch>, limit: usize) {
        results.clear();

        if query.is_empty() {
            for idx in (0..self.offsets.len()).rev() {
                results.push(FuzzyMatch {
                    entry_idx: idx,
                    match_positions: [0; 32],
                    match_count: 0,
                });
                if results.len() >= limit {
                    break;
                }
            }
            return;
        }

        let query_lower = lowercase_query(query);

        for (idx, &(start, len)) in self.offsets.iter().enumerate().rev() {
            let entry = &self.arena[start as usize..start as usize + len as usize];
            if let Some((positions, count)) = subsequence_match(&query_lower, entry) {
                results.push(FuzzyMatch {
                    entry_idx: idx,
                    match_positions: positions,
                    match_count: count,
                });
                if results.len() >= limit {
                    break;
                }
            }
        }
    }

    /// Get entry text by index.
    pub fn get(&self, idx: usize) -> &str {
        let (start, len) = self.offsets[idx];
        &self.arena[start as usize..start as usize + len as usize]
    }

    fn append_to_file(&mut self, line: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
            // Update file_pos so sync() doesn't re-read our own write
            if let Ok(m) = f.metadata() {
                self.file_pos = m.len();
            }
        }
    }
}

/// Lowercase a query into a fixed stack buffer, returning the used slice.
fn lowercase_query(query: &str) -> Vec<char> {
    query.chars().flat_map(|c| c.to_lowercase()).collect()
}

pub struct FuzzyMatch {
    pub entry_idx: usize,
    /// Matched character indices (as u16 — entries are always <64K chars).
    pub match_positions: [u16; 32],
    pub match_count: u8,
}

/// Check if `query` chars appear in `text` in order (case-insensitive).
/// Returns a fixed-size array of matched character indices and the count.
/// Zero heap allocations — uses stack arrays only.
pub fn subsequence_match(query: &[char], text: &str) -> Option<([u16; 32], u8)> {
    if query.is_empty() {
        return Some(([0; 32], 0));
    }

    let mut positions = [0u16; 32];

    // ASCII fast path: if both query and text are ASCII, operate on bytes directly.
    // This avoids char decoding and to_lowercase() overhead for the common case.
    if text.is_ascii() && query.iter().all(|c| c.is_ascii()) {
        let bytes = text.as_bytes();
        let mut qi = 0;
        let mut target = query[qi] as u8; // already lowercase from caller

        for (ti, &b) in bytes.iter().enumerate() {
            if b.to_ascii_lowercase() == target {
                positions[qi] = ti as u16;
                qi += 1;
                if qi == query.len() {
                    return Some((positions, qi as u8));
                }
                target = query[qi] as u8;
            }
        }
        return None;
    }

    let mut qi = 0;

    for (ti, tc) in text.chars().enumerate() {
        if tc.to_lowercase().next() == Some(query[qi]) {
            positions[qi] = ti as u16;
            qi += 1;
            if qi == query.len() {
                return Some((positions, qi as u8));
            }
        }
    }

    None
}

/// Count occurrences of a byte in a slice.
fn memchr_count(needle: u8, haystack: &[u8]) -> usize {
    haystack.iter().filter(|&&b| b == needle).count()
}

fn history_path() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data).join("ish/history")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/ish/history")
    } else {
        PathBuf::from("/tmp/ish_history")
    }
}

fn cache_path() -> PathBuf {
    let mut p = history_path();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".bin");
    p.set_file_name(name);
    p
}

fn lock_path() -> PathBuf {
    let mut p = history_path();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    p.set_file_name(name);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence() {
        let q: Vec<char> = "gco".chars().collect();
        let (positions, count) = subsequence_match(&q, "git checkout").unwrap();
        assert_eq!(count, 3);
        assert_eq!(&positions[..3], &[0, 4, 9]);
    }

    #[test]
    fn subsequence_no_match() {
        let q: Vec<char> = "xyz".chars().collect();
        assert!(subsequence_match(&q, "hello").is_none());
    }

    #[test]
    fn parallel_vecs_sync_after_add() {
        let entries: Vec<String> = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        let mut h = History::from_entries(entries);
        assert_eq!(h.offsets.len(), 3);
        assert_eq!(h.timestamps.len(), 3);

        // Add duplicate — should remove old and append new
        h.add("bbb");
        assert_eq!(h.offsets.len(), 3); // aaa, ccc, bbb
        assert_eq!(h.timestamps.len(), 3);
        assert_eq!(h.get(h.len() - 1), "bbb");

        // Add new
        h.add("ddd");
        assert_eq!(h.offsets.len(), 4);
        assert_eq!(h.timestamps.len(), 4);
    }

    #[test]
    fn timestamps_are_set() {
        let mut h = History::from_entries(vec!["old".into()]);
        let before = now_secs();
        h.add("new_cmd");
        let after = now_secs();
        let ts = h.timestamp(h.len() - 1);
        assert!(ts >= before && ts <= after);
    }

    #[test]
    fn v3_round_trip() {
        let entries: Vec<String> = vec!["ls -la".into(), "git status".into(), "cargo test".into()];
        let hist = History::from_entries(entries);

        // Serialize to v3 format
        let entry_count = hist.offsets.len();
        let mut arena_buf = Vec::new();
        for &(start, len) in &hist.offsets {
            arena_buf.extend_from_slice(
                &hist.arena.as_bytes()[start as usize..start as usize + len as usize],
            );
            arena_buf.push(0);
        }
        let arena_size = arena_buf.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(CACHE_MAGIC_V3);
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(arena_size as u32).to_le_bytes());
        for &ts in &hist.timestamps {
            buf.extend_from_slice(&ts.wrapping_sub(TS_EPOCH).to_le_bytes());
        }
        buf.extend_from_slice(&arena_buf);

        // Parse it back
        let path = PathBuf::from("/dev/null");
        let parsed = History::parse_v3(&buf, &path).expect("v3 round-trip failed");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.get(0), "ls -la");
        assert_eq!(parsed.get(1), "git status");
        assert_eq!(parsed.get(2), "cargo test");
        // Timestamps survive the round-trip
        for i in 0..3 {
            assert_eq!(parsed.timestamp(i), hist.timestamp(i));
        }
    }
}
