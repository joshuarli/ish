use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

const CACHE_MAGIC_V1: &[u8; 4] = b"ISH\x01";
const CACHE_MAGIC_V2: &[u8; 4] = b"ISH\x02";
const CACHE_HEADER_SIZE: usize = 4 + 8 + 4 + 4; // magic + reserved + entry_count + arena_size

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
    /// Ordered hashes parallel to `offsets` — for cache serialization.
    hash_vec: Vec<u64>,
    /// Epoch seconds when each entry was last used. Parallel to `offsets`.
    timestamps: Vec<u32>,
    /// Hash index for O(1) duplicate checks in add().
    hashes: HashSet<u64>,
    path: PathBuf,
}

impl History {
    pub fn load() -> Self {
        let path = history_path();

        // Try loading binary cache, then merge any new entries from the text file
        if let Some(mut hist) = Self::load_from_cache(&path) {
            hist.merge_text_tail();
            return hist;
        }

        // No cache or corrupt — full text parse, then write cache + truncate text
        let hist = Self::load_from_text(&path);
        if !hist.offsets.is_empty() {
            hist.save_cache();
        }

        hist
    }

    fn load_from_text(path: &Path) -> Self {
        let (arena, offsets, hash_vec, hashes) = match fs::read(path) {
            Ok(data) => {
                let line_count = memchr_count(b'\n', &data);
                let mut seen = HashSet::with_capacity(line_count);
                let mut deduped: Vec<&str> = Vec::with_capacity(line_count);
                let mut hashes_vec: Vec<u64> = Vec::with_capacity(line_count);

                for chunk in data.rsplit(|&b| b == b'\n') {
                    if let Ok(line) = std::str::from_utf8(chunk)
                        && !line.is_empty()
                    {
                        let h = hash_str(line);
                        if seen.insert(h) {
                            deduped.push(line);
                            hashes_vec.push(h);
                        }
                    }
                }
                deduped.reverse();
                hashes_vec.reverse();

                let total: usize = deduped.iter().map(|s| s.len()).sum();
                let mut arena = String::with_capacity(total);
                let mut offsets = Vec::with_capacity(deduped.len());
                for line in &deduped {
                    let start = arena.len() as u32;
                    arena.push_str(line);
                    offsets.push((start, line.len() as u16));
                }

                let hashes = hashes_vec.iter().copied().collect();
                (arena, offsets, hashes_vec, hashes)
            }
            Err(_) => (String::new(), Vec::new(), Vec::new(), HashSet::new()),
        };

        // No timestamps in text format — use current time for all entries
        let ts = now_secs();
        let timestamps = vec![ts; offsets.len()];

        Self {
            arena,
            offsets,
            hash_vec,
            timestamps,
            hashes,
            path: path.to_path_buf(),
        }
    }

    fn load_from_cache(path: &Path) -> Option<Self> {
        let data = fs::read(cache_path()).ok()?;

        if data.len() < CACHE_HEADER_SIZE {
            return None;
        }

        let version = match &data[0..4] {
            x if x == CACHE_MAGIC_V2 => 2,
            x if x == CACHE_MAGIC_V1 => 1,
            _ => return None,
        };

        // Skip reserved field (bytes 4..12)
        let entry_count = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
        let arena_size = u32::from_le_bytes(data[16..20].try_into().ok()?) as usize;

        let timestamps_size = if version >= 2 { entry_count * 4 } else { 0 };
        let expected =
            CACHE_HEADER_SIZE + entry_count * 8 + timestamps_size + entry_count * 6 + arena_size;
        if data.len() != expected {
            return None;
        }

        let mut pos = CACHE_HEADER_SIZE;

        // Read hashes
        let mut hash_vec = Vec::with_capacity(entry_count);
        let mut hashes = HashSet::with_capacity(entry_count);
        for _ in 0..entry_count {
            let h = u64::from_le_bytes(data[pos..pos + 8].try_into().ok()?);
            hash_vec.push(h);
            hashes.insert(h);
            pos += 8;
        }

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

        // Validate all offsets fall within the arena
        for &(start, len) in &offsets {
            if (start as usize) + (len as usize) > arena.len() {
                return None;
            }
        }

        Some(Self {
            arena,
            offsets,
            hash_vec,
            timestamps,
            hashes,
            path: path.to_path_buf(),
        })
    }

    /// Merge new entries from the text file (appended since last cache build).
    /// Deduplicates against existing hashes. Text entries win over cache
    /// entries (they're newer).
    fn merge_text_tail(&mut self) {
        let data = match fs::read(&self.path) {
            Ok(d) if !d.is_empty() => d,
            _ => return,
        };

        let ts = now_secs();

        // Parse tail entries — these are newer, so they take priority
        for chunk in data.split(|&b| b == b'\n') {
            if let Ok(line) = std::str::from_utf8(chunk)
                && !line.is_empty()
            {
                let h = hash_str(line);
                if self.hashes.contains(&h) {
                    // Duplicate — remove old occurrence, add at end
                    let mut i = 0;
                    while i < self.offsets.len() {
                        let (start, len) = self.offsets[i];
                        if &self.arena[start as usize..start as usize + len as usize] == line {
                            self.offsets.remove(i);
                            self.hash_vec.remove(i);
                            self.timestamps.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                }
                let start = self.arena.len() as u32;
                self.arena.push_str(line);
                self.offsets.push((start, line.len() as u16));
                self.hash_vec.push(h);
                self.timestamps.push(ts);
                self.hashes.insert(h);
            }
        }
    }

    /// Write binary cache (v2 with timestamps), then truncate text file.
    /// Atomic: writes cache to .tmp then renames.
    pub fn save_cache(&self) {
        let cache = cache_path();
        let tmp = cache.with_extension("bin.tmp");

        let entry_count = self.offsets.len();
        let arena_bytes = self.arena.as_bytes();
        let total = CACHE_HEADER_SIZE
            + entry_count * 8
            + entry_count * 4
            + entry_count * 6
            + arena_bytes.len();

        let mut buf = Vec::with_capacity(total);

        // Header (v2)
        buf.extend_from_slice(CACHE_MAGIC_V2);
        buf.extend_from_slice(&0u64.to_le_bytes()); // reserved
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(arena_bytes.len() as u32).to_le_bytes());

        // Hashes
        for &h in &self.hash_vec {
            buf.extend_from_slice(&h.to_le_bytes());
        }

        // Timestamps
        for &ts in &self.timestamps {
            buf.extend_from_slice(&ts.to_le_bytes());
        }

        // Offsets
        for &(start, len) in &self.offsets {
            buf.extend_from_slice(&start.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }

        // Arena
        buf.extend_from_slice(arena_bytes);

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
                self.save_cache();
                return;
            }
        };

        use std::os::fd::AsRawFd;
        // Try non-blocking lock — skip if another shell is compacting
        let rc = unsafe { libc::flock(lock_fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            self.save_cache();
            return;
        }

        // Re-read text tail from other shells, merge into our state
        self.merge_text_tail();

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
        let mut hash_vec = Vec::with_capacity(entries.len());
        let mut timestamps = Vec::with_capacity(entries.len());
        let mut hashes = HashSet::with_capacity(entries.len());
        for e in &entries {
            let start = arena.len() as u32;
            let h = hash_str(e);
            arena.push_str(e);
            offsets.push((start, e.len() as u16));
            hash_vec.push(h);
            timestamps.push(ts);
            hashes.insert(h);
        }
        Self {
            arena,
            offsets,
            hash_vec,
            timestamps,
            hashes,
            path: PathBuf::from("/dev/null"),
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
                    self.hash_vec.remove(i);
                    self.timestamps.remove(i);
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
        self.hash_vec.push(h);
        self.timestamps.push(now_secs());
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

    fn append_to_file(&self, line: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
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
    fn hash_vec_sync_after_add() {
        let entries: Vec<String> = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        let mut h = History::from_entries(entries);
        assert_eq!(h.hash_vec.len(), 3);
        assert_eq!(h.offsets.len(), 3);
        assert_eq!(h.timestamps.len(), 3);

        // Add duplicate — should remove old and append new
        h.add("bbb");
        assert_eq!(h.offsets.len(), 3); // aaa, ccc, bbb
        assert_eq!(h.hash_vec.len(), 3);
        assert_eq!(h.timestamps.len(), 3);
        assert_eq!(h.get(h.len() - 1), "bbb");

        // Add new
        h.add("ddd");
        assert_eq!(h.offsets.len(), 4);
        assert_eq!(h.hash_vec.len(), 4);
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
}
