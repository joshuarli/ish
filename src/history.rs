use fxhash::{FxHashMap, FxHashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

const CACHE_MAGIC_V1: &[u8; 4] = b"ISH\x01";
const CACHE_MAGIC_V2: &[u8; 4] = b"ISH\x02";
const CACHE_MAGIC_V3: &[u8; 4] = b"ISH\x03";
const CACHE_MAGIC_V4: &[u8; 4] = b"ISH\x04";
const CACHE_MAGIC_V5: &[u8; 4] = b"ISH\x05";
const LOG_RECORD_PREFIX: &str = ":ish-history:v1\t";
const LOG_RECORD_PREFIX_V2: &str = ":ish-history:v2\t";

/// v1/v2 header: magic(4) + reserved(8) + entry_count(4) + arena_size(4)
const V2_HEADER_SIZE: usize = 20;
/// v3 header: magic(4) + entry_count(4) + arena_size(4)
const V3_HEADER_SIZE: usize = 12;
const V5_HEADER_SIZE: usize = 16;

/// 1998-01-01T00:00:00 UTC as Unix epoch milliseconds.
const TS_EPOCH_MILLIS: u64 = 883_612_800_000;

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_session_id() -> u64 {
    now_millis().wrapping_shl(16) ^ rustix::process::getpid().as_raw_pid() as u64
}

struct ParsedHistoryLine<'a> {
    command: &'a str,
    timestamp: u64,
    session_id: u64,
    cwd: Option<PathBuf>,
}

fn parse_history_line<'a>(line: &'a str, fallback_ts: u64) -> Option<ParsedHistoryLine<'a>> {
    if line.is_empty() {
        return None;
    }

    if let Some(rest) = line.strip_prefix(LOG_RECORD_PREFIX_V2) {
        let mut parts = rest.splitn(4, '\t');
        if let (Some(ts), Some(session_id), Some(cwd), Some(command)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && !command.is_empty()
            && let (Ok(timestamp), Ok(session_id)) = (ts.parse::<u64>(), session_id.parse::<u64>())
        {
            return Some(ParsedHistoryLine {
                command,
                timestamp,
                session_id,
                cwd: unescape_history_field(cwd).map(PathBuf::from),
            });
        }
    }

    if let Some(rest) = line.strip_prefix(LOG_RECORD_PREFIX) {
        let mut parts = rest.splitn(3, '\t');
        if let (Some(ts), Some(session_id), Some(command)) =
            (parts.next(), parts.next(), parts.next())
            && !command.is_empty()
            && let (Ok(timestamp), Ok(session_id)) = (ts.parse::<u64>(), session_id.parse::<u64>())
        {
            return Some(ParsedHistoryLine {
                command,
                timestamp,
                session_id,
                cwd: None,
            });
        }
    }

    Some(ParsedHistoryLine {
        command: line,
        timestamp: fallback_ts,
        session_id: 0,
        cwd: None,
    })
}

fn format_history_record(timestamp: u64, session_id: u64, command: &str) -> String {
    format!("{LOG_RECORD_PREFIX}{timestamp}\t{session_id}\t{command}")
}

fn format_history_record_with_cwd(
    timestamp: u64,
    session_id: u64,
    cwd: &Path,
    command: &str,
) -> String {
    format!(
        "{LOG_RECORD_PREFIX_V2}{timestamp}\t{session_id}\t{}\t{command}",
        escape_history_field(&cwd.to_string_lossy())
    )
}

fn escape_history_field(field: &str) -> String {
    let mut escaped = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(c),
        }
    }
    escaped
}

fn unescape_history_field(field: &str) -> Option<String> {
    let mut unescaped = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            unescaped.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => unescaped.push('\\'),
            't' => unescaped.push('\t'),
            'n' => unescaped.push('\n'),
            'r' => unescaped.push('\r'),
            _ => return None,
        }
    }
    Some(unescaped)
}

pub struct History {
    /// All entry text packed into a single allocation.
    arena: String,
    /// (start, len) byte offsets into `arena` for each entry.
    offsets: Vec<(u32, u16)>,
    /// Epoch milliseconds when each entry was last used. Parallel to `offsets`.
    timestamps: Vec<u64>,
    /// Session ids parallel to `offsets`. Zero means unknown/legacy.
    session_ids: Vec<u64>,
    /// Working directory for each entry. None means unknown/legacy.
    cwds: Vec<Option<PathBuf>>,
    /// Maps command hash to its current in-memory position.
    index_by_hash: FxHashMap<u64, usize>,
    path: PathBuf,
    /// Byte offset into the text file we've read up to. Enables incremental
    /// sync — only new bytes appended by other shells are read.
    file_pos: u64,
    /// Per-entry flag: true if the entry was added by this shell session
    /// (`add()`).
    local: Vec<bool>,
    /// Entries with timestamps at or before this boundary are considered part
    /// of the session-visible history. Up-arrow sees those entries plus any
    /// entry added by this shell.
    session_cutoff: u64,
    /// Session id used for new entries written by this shell.
    session_id: u64,
    /// Set when the cache was corrupt at load time. Prevents overwriting the
    /// (possibly recoverable) cache file until the user resolves it.
    cache_dirty: bool,
}

impl History {
    pub fn load() -> Self {
        Self::load_from(history_path())
    }

    pub fn load_from(path: PathBuf) -> Self {
        let cache = cache_path_for(&path);

        match Self::load_from_cache(&path, &cache) {
            Ok(Some(mut hist)) => {
                // Cache loaded — sync any new entries from the text file
                hist.sync();
                hist.session_cutoff = now_millis();
                hist
            }
            Ok(None) => {
                // No cache file (first launch) — build from text, write cache
                let mut hist = Self::load_from_text(&path);
                hist.file_pos = fs::metadata(&hist.path).map(|m| m.len()).unwrap_or(0);
                if !hist.offsets.is_empty() {
                    hist.save_cache();
                }
                hist.session_cutoff = now_millis();
                hist
            }
            Err(()) => {
                // Cache corrupt — load text file but do NOT write cache or
                // truncate the text file. The corrupt cache is left for the
                // user to inspect/delete manually.
                let mut hist = Self::load_from_text(&path);
                hist.file_pos = fs::metadata(&hist.path).map(|m| m.len()).unwrap_or(0);
                hist.cache_dirty = true;
                hist.session_cutoff = now_millis();
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
        fresh.session_cutoff = now_millis();
        fresh.session_id = self.session_id;
        fresh.save_cache();
        eprintln!(
            "ish: rebuilt history cache — {} entries",
            fresh.offsets.len()
        );
        *self = fresh;
    }

    fn load_from_text(path: &Path) -> Self {
        let (arena, offsets, timestamps, session_ids, cwds, index_by_hash) = match fs::read(path) {
            Ok(data) => {
                let line_count = memchr_count(b'\n', &data);
                let mut seen = FxHashSet::with_capacity_and_hasher(line_count, Default::default());
                let mut deduped: Vec<(String, u64, u64, Option<PathBuf>)> =
                    Vec::with_capacity(line_count);
                let fallback_ts = now_millis();

                for chunk in data.rsplit(|&b| b == b'\n') {
                    if let Ok(line) = std::str::from_utf8(chunk)
                        && let Some(parsed) = parse_history_line(line, fallback_ts)
                    {
                        let h = hash_str(parsed.command);
                        if seen.insert(h) {
                            deduped.push((
                                parsed.command.to_string(),
                                parsed.timestamp,
                                parsed.session_id,
                                parsed.cwd,
                            ));
                        }
                    }
                }
                deduped.reverse();

                let total: usize = deduped.iter().map(|(s, _, _, _)| s.len()).sum();
                let mut arena = String::with_capacity(total);
                let mut offsets = Vec::with_capacity(deduped.len());
                let mut timestamps = Vec::with_capacity(deduped.len());
                let mut session_ids = Vec::with_capacity(deduped.len());
                let mut cwds = Vec::with_capacity(deduped.len());
                for (line, timestamp, session_id, cwd) in &deduped {
                    let start = arena.len() as u32;
                    arena.push_str(line);
                    offsets.push((start, line.len() as u16));
                    timestamps.push(*timestamp);
                    session_ids.push(*session_id);
                    cwds.push(cwd.clone());
                }

                let mut index_by_hash =
                    FxHashMap::with_capacity_and_hasher(deduped.len(), Default::default());
                for (idx, (line, _, _, _)) in deduped.iter().enumerate() {
                    index_by_hash.insert(hash_str(line), idx);
                }

                (arena, offsets, timestamps, session_ids, cwds, index_by_hash)
            }
            Err(_) => (
                String::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                FxHashMap::default(),
            ),
        };
        let count = offsets.len();

        Self {
            arena,
            offsets,
            timestamps,
            session_ids,
            cwds,
            index_by_hash,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![false; count],
            session_cutoff: 0,
            session_id: new_session_id(),
            cache_dirty: false,
        }
    }

    /// Returns `Ok(Some)` on success, `Ok(None)` if no cache file exists,
    /// `Err(())` if the cache exists but is corrupt or unreadable.
    fn load_from_cache(path: &Path, cache: &Path) -> Result<Option<Self>, ()> {
        let data = match fs::read(cache) {
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
            x if x == CACHE_MAGIC_V5 => Self::parse_v5(data, path),
            x if x == CACHE_MAGIC_V4 => Self::parse_v4(data, path),
            x if x == CACHE_MAGIC_V3 => Self::parse_v3(data, path),
            x if x == CACHE_MAGIC_V2 => Self::parse_v1v2(data, path, 2),
            x if x == CACHE_MAGIC_V1 => Self::parse_v1v2(data, path, 1),
            _ => None,
        }
    }

    /// Parse v5 format: [magic(4)][entry_count(4)][arena_size(4)][cwd_arena_size(4)]
    /// [timestamps: N×8][arena: \0-delimited][cwd arena: \0-delimited]
    fn parse_v5(data: &[u8], path: &Path) -> Option<Self> {
        if data.len() < V5_HEADER_SIZE {
            return None;
        }

        let entry_count = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
        let arena_size = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        let cwd_arena_size = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
        let expected = V5_HEADER_SIZE
            .checked_add(entry_count.checked_mul(8)?)?
            .checked_add(arena_size)?
            .checked_add(cwd_arena_size)?;
        if data.len() != expected {
            return None;
        }

        let ts_start = V5_HEADER_SIZE;
        let mut timestamps = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let off = ts_start + i * 8;
            let stored = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
            timestamps.push(stored.wrapping_add(TS_EPOCH_MILLIS));
        }

        let arena_start = ts_start + entry_count * 8;
        let cwd_start = arena_start + arena_size;
        let cwd_arena = std::str::from_utf8(&data[cwd_start..cwd_start + cwd_arena_size]).ok()?;
        let mut cwd_parts = cwd_arena.split('\0');
        let mut cwds = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let cwd = cwd_parts.next()?;
            cwds.push((!cwd.is_empty()).then(|| PathBuf::from(cwd)));
        }
        if cwd_parts.next() != Some("") || cwd_parts.next().is_some() {
            return None;
        }

        Self::parse_delimited_arena(path, data, arena_start, arena_size, timestamps, cwds)
    }

    /// Parse v4 format: [magic(4)][entry_count(4)][arena_size(4)][timestamps: N×8][arena: \0-delimited]
    fn parse_v4(data: &[u8], path: &Path) -> Option<Self> {
        if data.len() < V3_HEADER_SIZE {
            return None;
        }

        let entry_count = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
        let arena_size = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;

        let expected = V3_HEADER_SIZE + entry_count * 8 + arena_size;
        if data.len() != expected {
            return None;
        }

        let ts_start = V3_HEADER_SIZE;
        let mut timestamps = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let off = ts_start + i * 8;
            let stored = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
            timestamps.push(stored.wrapping_add(TS_EPOCH_MILLIS));
        }

        Self::parse_delimited_arena(
            path,
            data,
            ts_start + entry_count * 8,
            arena_size,
            timestamps,
            vec![None; entry_count],
        )
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
            timestamps.push(stored as u64 * 1000 + TS_EPOCH_MILLIS);
        }

        Self::parse_delimited_arena(
            path,
            data,
            ts_start + entry_count * 4,
            arena_size,
            timestamps,
            vec![None; entry_count],
        )
    }

    fn parse_delimited_arena(
        path: &Path,
        data: &[u8],
        arena_start: usize,
        arena_size: usize,
        timestamps: Vec<u64>,
        cwds: Vec<Option<PathBuf>>,
    ) -> Option<Self> {
        let entry_count = timestamps.len();
        if cwds.len() != entry_count {
            return None;
        }
        let arena_bytes = &data[arena_start..arena_start + arena_size];
        let arena_str = std::str::from_utf8(arena_bytes).ok()?;

        let mut arena = String::with_capacity(arena_size);
        let mut offsets = Vec::with_capacity(entry_count);
        let mut index_by_hash =
            FxHashMap::with_capacity_and_hasher(entry_count, Default::default());
        let mut count = 0;
        for entry in arena_str.split('\0') {
            if entry.is_empty() {
                continue;
            }
            let start = arena.len() as u32;
            arena.push_str(entry);
            offsets.push((start, entry.len() as u16));
            index_by_hash.insert(hash_str(entry), count);
            count += 1;
        }

        if count != entry_count {
            return None;
        }

        Some(Self {
            arena,
            offsets,
            timestamps,
            session_ids: vec![0; count],
            cwds,
            index_by_hash,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![false; count],
            session_cutoff: 0,
            session_id: new_session_id(),
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
        let mut index_by_hash =
            FxHashMap::with_capacity_and_hasher(entry_count, Default::default());
        pos += entry_count * 8;

        // Read timestamps (v2+) or default to 0 (v1)
        let timestamps = if version >= 2 {
            let mut ts = Vec::with_capacity(entry_count);
            for _ in 0..entry_count {
                ts.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as u64 * 1000);
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
        }
        for (idx, &(start, len)) in offsets.iter().enumerate() {
            index_by_hash.insert(
                hash_str(&arena[start as usize..start as usize + len as usize]),
                idx,
            );
        }

        let count = offsets.len();
        Some(Self {
            arena,
            offsets,
            timestamps,
            session_ids: vec![0; count],
            cwds: vec![None; count],
            index_by_hash,
            path: path.to_path_buf(),
            file_pos: 0,
            local: vec![false; count],
            session_cutoff: 0,
            session_id: new_session_id(),
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

        let ts = now_millis();
        for line in tail.lines() {
            let Some(parsed) = parse_history_line(line, ts) else {
                continue;
            };
            let h = hash_str(parsed.command);
            if let Some(idx) = self.find_entry_index(h, parsed.command) {
                // Don't let a newer hidden duplicate disturb the entries this
                // session can still recall with Up-arrow.
                if self.is_session_visible(idx) {
                    continue;
                }
                self.remove_entry_at(idx);
            }
            let start = self.arena.len() as u32;
            self.arena.push_str(parsed.command);
            self.offsets.push((start, parsed.command.len() as u16));
            self.timestamps.push(parsed.timestamp);
            self.session_ids.push(parsed.session_id);
            self.cwds.push(parsed.cwd);
            self.local.push(false);
            self.index_by_hash.insert(h, self.offsets.len() - 1);
        }

        self.file_pos = file_size;
    }

    /// Write v5 binary cache, then truncate text file.
    /// v5 format: [magic(4)][entry_count(4)][arena_size(4)][cwd_arena_size(4)]
    /// [timestamps: N×8][arena: \0-delimited][cwd arena: \0-delimited]
    /// Atomic: writes cache to .tmp then renames.
    pub fn save_cache(&self) {
        if self.cache_dirty {
            return;
        }
        let cache = cache_path_for(&self.path);
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

        let mut cwd_arena_buf = Vec::new();
        for cwd in &self.cwds {
            if let Some(cwd) = cwd {
                cwd_arena_buf.extend_from_slice(cwd.to_string_lossy().as_bytes());
            }
            cwd_arena_buf.push(0);
        }
        let cwd_arena_size = cwd_arena_buf.len();

        let total = V5_HEADER_SIZE + entry_count * 8 + arena_size + cwd_arena_size;
        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(CACHE_MAGIC_V5);
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(arena_size as u32).to_le_bytes());
        buf.extend_from_slice(&(cwd_arena_size as u32).to_le_bytes());

        // Timestamps (offset from 1998 epoch)
        for &ts in &self.timestamps {
            buf.extend_from_slice(&ts.wrapping_sub(TS_EPOCH_MILLIS).to_le_bytes());
        }

        // Null-delimited arena
        buf.extend_from_slice(&arena_buf);
        buf.extend_from_slice(&cwd_arena_buf);

        // Guard: refuse to overwrite a larger cache with a much smaller one.
        if let Ok(existing) = fs::read(&cache)
            && existing.len() >= 4
        {
            let old_count = match &existing[0..4] {
                x if x == CACHE_MAGIC_V5 && existing.len() >= V5_HEADER_SIZE => {
                    u32::from_le_bytes(existing[4..8].try_into().unwrap_or_default()) as usize
                }
                x if x == CACHE_MAGIC_V4 && existing.len() >= V3_HEADER_SIZE => {
                    u32::from_le_bytes(existing[4..8].try_into().unwrap_or_default()) as usize
                }
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
        let lock = lock_path_for(&self.path);
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

        // Try non-blocking lock — skip if another shell is compacting
        let lock_result = rustix::fs::flock(
            &lock_fd,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        );
        if lock_result.is_err() {
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
        let ts = now_millis();
        let total: usize = entries.iter().map(|e| e.len()).sum();
        let mut arena = String::with_capacity(total);
        let mut offsets = Vec::with_capacity(entries.len());
        let mut timestamps = Vec::with_capacity(entries.len());
        let mut session_ids = Vec::with_capacity(entries.len());
        let mut cwds = Vec::with_capacity(entries.len());
        let mut index_by_hash =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for e in &entries {
            let start = arena.len() as u32;
            let h = hash_str(e);
            arena.push_str(e);
            offsets.push((start, e.len() as u16));
            timestamps.push(ts);
            session_ids.push(0);
            cwds.push(None);
            index_by_hash.insert(h, offsets.len() - 1);
        }
        let count = offsets.len();
        Self {
            arena,
            offsets,
            timestamps,
            session_ids,
            cwds,
            index_by_hash,
            path: PathBuf::from("/dev/null"),
            file_pos: 0,
            local: vec![false; count],
            session_cutoff: ts,
            session_id: new_session_id(),
            cache_dirty: false,
        }
    }

    /// Add entry. Deduplicates (removes prior occurrence).
    pub fn add(&mut self, line: &str) {
        let cwd = std::env::current_dir().ok();
        self.add_in_dir(line, cwd.as_deref());
    }

    /// Add an entry with the directory in which it was entered.
    pub fn add_in_dir(&mut self, line: &str, cwd: Option<&Path>) {
        // Collapse newlines to spaces to prevent history file corruption.
        let line = line.trim().replace('\n', " ");
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        let h = hash_str(line);
        if self.index_by_hash.contains_key(&h) {
            self.remove_entries_matching(line);
        }
        // Truncate entries that exceed u16 max (64KB) — shouldn't happen in practice
        let len = line.len().min(u16::MAX as usize);
        let start = self.arena.len() as u32;
        self.arena.push_str(&line[..len]);
        self.offsets.push((start, len as u16));
        self.timestamps.push(now_millis());
        self.session_ids.push(self.session_id);
        self.cwds.push(cwd.map(Path::to_path_buf));
        self.local.push(true);
        self.index_by_hash.insert(h, self.offsets.len() - 1);

        // Append to file
        self.append_to_file(line, cwd);
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Get the timestamp (epoch milliseconds) for entry at index.
    pub fn timestamp(&self, idx: usize) -> u64 {
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

    /// Get the `skip`'th session-visible entry from the end (for up-arrow
    /// navigation). Session-visible entries are those present when the shell
    /// started plus those added by this shell.
    pub fn session_get(&self, skip: usize) -> Option<&str> {
        self.offsets
            .iter()
            .enumerate()
            .rev()
            .filter(|&(i, _)| self.is_session_visible(i))
            .nth(skip)
            .map(|(_, &(start, len))| &self.arena[start as usize..start as usize + len as usize])
    }

    /// Prefix search over session-visible entries only (for up-arrow with
    /// partial input).
    pub fn session_prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.offsets
            .iter()
            .enumerate()
            .rev()
            .filter(|&(i, _)| self.is_session_visible(i))
            .filter_map(|(_, &(start, len))| {
                let s = &self.arena[start as usize..start as usize + len as usize];
                s.starts_with(prefix).then_some(s)
            })
            .nth(skip)
    }

    /// History search used by Ctrl+R.
    ///
    /// Ranking is intentionally simple and recency-friendly:
    /// 1. prefix match
    /// 2. substring match at a word boundary
    /// 3. other substring match
    /// 4. subsequence fallback
    ///
    /// Within a tier, newer entries win.
    pub fn fuzzy_search(&self, query: &str) -> Vec<FuzzyMatch> {
        self.fuzzy_search_scored(query, "")
    }

    /// Like `fuzzy_search` but keeps the old signature used by callers/tests.
    pub fn fuzzy_search_scored(&self, query: &str, cwd: &str) -> Vec<FuzzyMatch> {
        let mut results = Vec::new();
        let cwd = (!cwd.is_empty()).then(|| Path::new(cwd));
        self.fill_search_results(query, &mut results, cwd);
        results
    }

    /// Search with a priority boost for entries recorded in an ancestor directory.
    pub fn fuzzy_search_in_dir(&self, query: &str, cwd: &Path) -> Vec<FuzzyMatch> {
        let mut results = Vec::new();
        self.fill_search_results(query, &mut results, Some(cwd));
        results
    }

    /// Like `fuzzy_search` but appends into a caller-owned Vec (zero-alloc reuse).
    /// Caps at `limit` results since the pager only shows a screenful.
    pub fn fuzzy_search_into(
        &self,
        query: &str,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: &str,
    ) {
        let cwd = (!cwd.is_empty()).then(|| Path::new(cwd));
        self.fuzzy_search_into_with_cwd(query, results, limit, cwd);
    }

    /// Search with cwd weighting into a caller-owned result buffer.
    pub fn fuzzy_search_into_in_dir(
        &self,
        query: &str,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: &Path,
    ) {
        self.fuzzy_search_into_with_cwd(query, results, limit, Some(cwd));
    }

    fn fuzzy_search_into_with_cwd(
        &self,
        query: &str,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        if query.is_ascii() && query.len() <= 32 {
            let mut query_lower = [0u8; 32];
            for (slot, byte) in query_lower.iter_mut().zip(query.bytes()) {
                *slot = byte.to_ascii_lowercase();
            }
            self.fill_search_results_limited_bytes(
                &query_lower[..query.len()],
                results,
                limit,
                cwd,
            );
        } else {
            self.fill_search_results_limited(query, results, limit, cwd);
        }
    }

    /// Fill `out` with session-visible entry indices in recency order.
    pub fn visible_entry_indices_into(&self, out: &mut Vec<usize>) {
        out.clear();
        out.extend(
            (0..self.offsets.len())
                .rev()
                .filter(|&idx| self.is_session_visible(idx)),
        );
    }

    /// Search within an existing candidate set, preserving all matches in
    /// `matched_indices` and the best `limit` ranked results in `results`.
    pub fn fuzzy_search_subset_into(
        &self,
        query: &str,
        candidates: &[usize],
        matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
    ) {
        self.fuzzy_search_subset_into_with_cwd(
            query,
            candidates,
            matched_indices,
            results,
            limit,
            None,
        );
    }

    /// Search a candidate set with a priority boost for entries recorded in an
    /// ancestor of `cwd`.
    pub fn fuzzy_search_subset_into_in_dir(
        &self,
        query: &str,
        candidates: &[usize],
        matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: &Path,
    ) {
        self.fuzzy_search_subset_into_with_cwd(
            query,
            candidates,
            matched_indices,
            results,
            limit,
            Some(cwd),
        );
    }

    fn fuzzy_search_subset_into_with_cwd(
        &self,
        query: &str,
        candidates: &[usize],
        matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        matched_indices.clear();
        results.clear();
        if limit == 0 {
            return;
        }

        if query.is_empty() {
            matched_indices.extend_from_slice(candidates);
            results.extend(candidates.iter().map(|&idx| FuzzyMatch {
                entry_idx: idx,
                match_positions: [0; 32],
                match_count: 0,
                score: self.cwd_weight(idx, cwd),
            }));
            if cwd.is_some() {
                results.sort_unstable_by(compare_fuzzy_match);
            }
            results.truncate(limit);
            return;
        }

        let query_lower = lowercase_query(query);
        for &idx in candidates {
            let entry = self.entry_text(idx);
            let Some(mut m) = classify_match(&query_lower, entry, idx) else {
                continue;
            };
            m.score += self.cwd_weight(idx, cwd);
            matched_indices.push(idx);
            let insert_at = results
                .binary_search_by(|existing| compare_fuzzy_match(existing, &m))
                .unwrap_or_else(|pos| pos);
            if insert_at >= limit {
                continue;
            }
            results.insert(insert_at, m);
            if results.len() > limit {
                results.pop();
            }
            if results.len() == limit && self.can_stop_search(results, cwd) {
                break;
            }
        }
    }

    /// Get entry text by index.
    pub fn get(&self, idx: usize) -> &str {
        let (start, len) = self.offsets[idx];
        &self.arena[start as usize..start as usize + len as usize]
    }

    /// Write all entries to the text file so a forked child can read them.
    /// Used before `history > file` or `history | cmd`.
    pub fn flush_for_read(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            use std::io::Write;
            for (i, &(start, len)) in self.offsets.iter().enumerate() {
                let entry = &self.arena[start as usize..start as usize + len as usize];
                let record = self.cwds[i].as_deref().map_or_else(
                    || format_history_record(self.timestamps[i], self.session_ids[i], entry),
                    |cwd| {
                        format_history_record_with_cwd(
                            self.timestamps[i],
                            self.session_ids[i],
                            cwd,
                            entry,
                        )
                    },
                );
                let _ = writeln!(f, "{record}");
            }
        }
    }

    fn append_to_file(&mut self, line: &str, cwd: Option<&Path>) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let record = cwd.map_or_else(
                || format_history_record(now_millis(), self.session_id, line),
                |cwd| format_history_record_with_cwd(now_millis(), self.session_id, cwd, line),
            );
            let _ = writeln!(f, "{record}");
            // Update file_pos so sync() doesn't re-read our own write
            if let Ok(m) = f.metadata() {
                self.file_pos = m.len();
            }
        }
    }

    fn fill_search_results(&self, query: &str, results: &mut Vec<FuzzyMatch>, cwd: Option<&Path>) {
        results.clear();

        if query.is_empty() {
            results.extend(
                (0..self.offsets.len())
                    .rev()
                    .filter(|&idx| self.is_session_visible(idx))
                    .map(|idx| FuzzyMatch {
                        entry_idx: idx,
                        match_positions: [0; 32],
                        match_count: 0,
                        score: self.cwd_weight(idx, cwd),
                    }),
            );
            if cwd.is_some() {
                results.sort_unstable_by(compare_fuzzy_match);
            }
            return;
        }

        let query_lower = lowercase_query(query);
        for (idx, &(start, len)) in self.offsets.iter().enumerate().rev() {
            if !self.is_session_visible(idx) {
                continue;
            }
            let entry = &self.arena[start as usize..start as usize + len as usize];
            if let Some(mut m) = classify_match(&query_lower, entry, idx) {
                m.score += self.cwd_weight(idx, cwd);
                results.push(m);
            }
        }

        results.sort_unstable_by(compare_fuzzy_match);
    }

    fn fill_search_results_limited(
        &self,
        query: &str,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        let query_lower = lowercase_query(query);
        self.fill_search_results_limited_chars(&query_lower, results, limit, cwd);
    }

    fn fill_search_results_limited_bytes(
        &self,
        query_lower: &[u8],
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        results.clear();
        if limit == 0 {
            return;
        }

        if query_lower.is_empty() {
            results.extend(
                (0..self.offsets.len())
                    .rev()
                    .filter(|&idx| self.is_session_visible(idx))
                    .take(limit)
                    .map(|idx| FuzzyMatch {
                        entry_idx: idx,
                        match_positions: [0; 32],
                        match_count: 0,
                        score: self.cwd_weight(idx, cwd),
                    }),
            );
            if cwd.is_some() {
                results.sort_unstable_by(compare_fuzzy_match);
                results.truncate(limit);
            }
            return;
        }

        for (idx, &(start, len)) in self.offsets.iter().enumerate().rev() {
            if !self.is_session_visible(idx) {
                continue;
            }
            let entry = &self.arena[start as usize..start as usize + len as usize];
            let Some(mut m) = classify_match_ascii(query_lower, entry, idx) else {
                continue;
            };
            m.score += self.cwd_weight(idx, cwd);

            let insert_at = results
                .binary_search_by(|existing| compare_fuzzy_match(existing, &m))
                .unwrap_or_else(|pos| pos);
            if insert_at >= limit {
                continue;
            }
            results.insert(insert_at, m);
            if results.len() > limit {
                results.pop();
            }
            if results.len() == limit && self.can_stop_search(results, cwd) {
                break;
            }
        }
    }

    fn fill_search_results_limited_chars(
        &self,
        query_lower: &[char],
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        results.clear();
        if limit == 0 {
            return;
        }

        if query_lower.is_empty() {
            results.extend(
                (0..self.offsets.len())
                    .rev()
                    .filter(|&idx| self.is_session_visible(idx))
                    .take(limit)
                    .map(|idx| FuzzyMatch {
                        entry_idx: idx,
                        match_positions: [0; 32],
                        match_count: 0,
                        score: self.cwd_weight(idx, cwd),
                    }),
            );
            if cwd.is_some() {
                results.sort_unstable_by(compare_fuzzy_match);
                results.truncate(limit);
            }
            return;
        }

        for (idx, &(start, len)) in self.offsets.iter().enumerate().rev() {
            if !self.is_session_visible(idx) {
                continue;
            }
            let entry = &self.arena[start as usize..start as usize + len as usize];
            let Some(mut m) = classify_match(query_lower, entry, idx) else {
                continue;
            };
            m.score += self.cwd_weight(idx, cwd);

            let insert_at = results
                .binary_search_by(|existing| compare_fuzzy_match(existing, &m))
                .unwrap_or_else(|pos| pos);
            if insert_at >= limit {
                continue;
            }
            results.insert(insert_at, m);
            if results.len() > limit {
                results.pop();
            }
            if results.len() == limit && self.can_stop_search(results, cwd) {
                break;
            }
        }
    }

    fn cwd_weight(&self, idx: usize, cwd: Option<&Path>) -> i16 {
        const CWD_WEIGHT: i16 = 4;
        if cwd.is_some_and(|cwd| {
            self.cwds[idx]
                .as_deref()
                .is_some_and(|entry_cwd| cwd.starts_with(entry_cwd))
        }) { CWD_WEIGHT } else { 0 }
    }

    fn can_stop_search(&self, results: &[FuzzyMatch], cwd: Option<&Path>) -> bool {
        let best_possible_score = if cwd.is_some() { 7 } else { 3 };
        results
            .last()
            .is_some_and(|m| m.score >= best_possible_score)
    }

    fn is_session_visible(&self, idx: usize) -> bool {
        self.local[idx] || self.timestamps[idx] <= self.session_cutoff
    }

    fn entry_text(&self, idx: usize) -> &str {
        let (start, len) = self.offsets[idx];
        &self.arena[start as usize..start as usize + len as usize]
    }

    fn find_entry_index(&self, hash: u64, text: &str) -> Option<usize> {
        self.index_by_hash
            .get(&hash)
            .copied()
            .filter(|&idx| self.entry_text(idx) == text)
            .or_else(|| {
                self.offsets
                    .iter()
                    .enumerate()
                    .find_map(|(idx, _)| (self.entry_text(idx) == text).then_some(idx))
            })
    }

    fn remove_entry_at(&mut self, idx: usize) {
        self.offsets.remove(idx);
        self.timestamps.remove(idx);
        self.session_ids.remove(idx);
        self.cwds.remove(idx);
        self.local.remove(idx);
        self.rebuild_index();
    }

    fn remove_entries_matching(&mut self, text: &str) {
        let mut matches = Vec::new();
        for idx in 0..self.offsets.len() {
            if self.entry_text(idx) == text {
                matches.push(idx);
            }
        }
        for idx in matches.into_iter().rev() {
            self.offsets.remove(idx);
            self.timestamps.remove(idx);
            self.session_ids.remove(idx);
            self.cwds.remove(idx);
            self.local.remove(idx);
        }
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.index_by_hash.clear();
        for idx in 0..self.offsets.len() {
            self.index_by_hash
                .insert(hash_str(self.entry_text(idx)), idx);
        }
    }
}

fn compare_fuzzy_match(a: &FuzzyMatch, b: &FuzzyMatch) -> std::cmp::Ordering {
    b.score.cmp(&a.score).then(b.entry_idx.cmp(&a.entry_idx))
}

pub fn render_history_file(path: &Path) -> std::io::Result<String> {
    let data = fs::read(path)?;
    let fallback_ts = now_millis();
    let mut out = String::new();
    for chunk in data.split(|&b| b == b'\n') {
        if let Ok(line) = std::str::from_utf8(chunk)
            && let Some(parsed) = parse_history_line(line, fallback_ts)
        {
            out.push_str(parsed.command);
            out.push('\n');
        }
    }
    Ok(out)
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
    /// Match tier. Higher = stronger literal match.
    /// 3 = prefix, 2 = boundary substring, 1 = substring, 0 = subsequence fallback.
    pub score: i16,
}

fn classify_match(query: &[char], text: &str, entry_idx: usize) -> Option<FuzzyMatch> {
    if starts_with_icase(query, text) {
        return Some(contiguous_match(entry_idx, 3, 0, query.len()));
    }

    if let Some(start) = find_substring_icase(query, text, true) {
        return Some(contiguous_match(entry_idx, 2, start, query.len()));
    }

    if let Some(start) = find_substring_icase(query, text, false) {
        return Some(contiguous_match(entry_idx, 1, start, query.len()));
    }

    let (positions, count) = subsequence_match(query, text)?;
    Some(FuzzyMatch {
        entry_idx,
        match_positions: positions,
        match_count: count,
        score: 0,
    })
}

fn classify_match_ascii(query: &[u8], text: &str, entry_idx: usize) -> Option<FuzzyMatch> {
    if starts_with_icase_ascii(query, text.as_bytes()) {
        return Some(contiguous_match(entry_idx, 3, 0, query.len()));
    }

    if let Some(start) = find_substring_icase_ascii_bytes(query, text.as_bytes(), true) {
        return Some(contiguous_match(entry_idx, 2, start, query.len()));
    }

    if let Some(start) = find_substring_icase_ascii_bytes(query, text.as_bytes(), false) {
        return Some(contiguous_match(entry_idx, 1, start, query.len()));
    }

    let (positions, count) = subsequence_match_ascii_bytes(query, text.as_bytes())?;
    Some(FuzzyMatch {
        entry_idx,
        match_positions: positions,
        match_count: count,
        score: 0,
    })
}

fn contiguous_match(entry_idx: usize, score: i16, start: usize, len: usize) -> FuzzyMatch {
    let mut positions = [0u16; 32];
    let count = len.min(positions.len()).min(u8::MAX as usize);
    for (offset, slot) in positions.iter_mut().take(count).enumerate() {
        *slot = (start + offset) as u16;
    }
    FuzzyMatch {
        entry_idx,
        match_positions: positions,
        match_count: count as u8,
        score,
    }
}

fn starts_with_icase(query: &[char], text: &str) -> bool {
    let mut chars = text.chars();
    for &q in query {
        let Some(tc) = chars.next() else {
            return false;
        };
        if tc.to_lowercase().next() != Some(q) {
            return false;
        }
    }
    true
}

fn starts_with_icase_ascii(query: &[u8], text: &[u8]) -> bool {
    text.len() >= query.len()
        && text[..query.len()]
            .iter()
            .zip(query)
            .all(|(&text_byte, &query_byte)| text_byte.to_ascii_lowercase() == query_byte)
}

fn find_substring_icase(query: &[char], text: &str, boundary_only: bool) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }

    if text.is_ascii() && query.iter().all(|c| c.is_ascii()) {
        return find_substring_icase_ascii(query, text.as_bytes(), boundary_only);
    }

    let chars: Vec<char> = text.chars().collect();
    if query.len() > chars.len() {
        return None;
    }

    for start in 0..=chars.len() - query.len() {
        if boundary_only && start > 0 && !is_word_boundary_char(chars[start - 1]) {
            continue;
        }
        if chars[start..start + query.len()]
            .iter()
            .zip(query.iter())
            .all(|(&tc, &q)| tc.to_lowercase().next() == Some(q))
        {
            return Some(start);
        }
    }

    None
}

fn find_substring_icase_ascii(query: &[char], text: &[u8], boundary_only: bool) -> Option<usize> {
    if query.len() > text.len() {
        return None;
    }

    'start: for start in 0..=text.len() - query.len() {
        if boundary_only && start > 0 && !is_word_boundary_byte(text[start - 1]) {
            continue;
        }
        for (offset, &q) in query.iter().enumerate() {
            if text[start + offset].to_ascii_lowercase() != q as u8 {
                continue 'start;
            }
        }
        return Some(start);
    }

    None
}

fn find_substring_icase_ascii_bytes(
    query: &[u8],
    text: &[u8],
    boundary_only: bool,
) -> Option<usize> {
    if query.len() > text.len() {
        return None;
    }

    'start: for start in 0..=text.len() - query.len() {
        if boundary_only && start > 0 && !is_word_boundary_byte(text[start - 1]) {
            continue;
        }
        for (offset, &query_byte) in query.iter().enumerate() {
            if text[start + offset].to_ascii_lowercase() != query_byte {
                continue 'start;
            }
        }
        return Some(start);
    }

    None
}

/// Check if `query` chars appear in `text` in order (case-insensitive).
/// Uses a forward-then-backward scan to find the tightest match window,
/// then a final forward pass within that window for optimal positions.
/// Returns a fixed-size array of matched character indices and the count.
/// Zero heap allocations — uses stack arrays only.
pub fn subsequence_match(query: &[char], text: &str) -> Option<([u16; 32], u8)> {
    if query.is_empty() {
        return Some(([0; 32], 0));
    }

    // ASCII fast path: if both query and text are ASCII, operate on bytes directly.
    if text.is_ascii() && query.iter().all(|c| c.is_ascii()) {
        return subsequence_match_ascii(query, text);
    }

    subsequence_match_unicode(query, text)
}

/// ASCII fast path — operates on bytes directly, no char decoding.
fn subsequence_match_ascii(query: &[char], text: &str) -> Option<([u16; 32], u8)> {
    let bytes = text.as_bytes();
    let qlen = query.len();
    let last_qchar = query[qlen - 1] as u8;

    // 1) Forward pass: find the first complete match to confirm it exists.
    let mut qi = 0;
    let mut first_end = 0usize; // index of the first endpoint (last query char match)
    for (ti, &b) in bytes.iter().enumerate() {
        if b.to_ascii_lowercase() == query[qi] as u8 {
            qi += 1;
            if qi == qlen {
                first_end = ti;
                break;
            }
        }
    }
    if qi < qlen {
        return None;
    }

    // 2) Find the last occurrence of the last query char beyond the first endpoint.
    let mut last_end = first_end;
    for (ti, &b) in bytes.iter().enumerate().skip(first_end + 1) {
        if b.to_ascii_lowercase() == last_qchar {
            last_end = ti;
        }
    }

    // 3) Backward pass from both endpoints; pick the tighter window.
    let (window_start, window_end) = if last_end == first_end {
        (backward_ascii(bytes, query, first_end), first_end)
    } else {
        let start1 = backward_ascii(bytes, query, first_end);
        let start2 = backward_ascii(bytes, query, last_end);
        let span1 = first_end - start1;
        let span2 = last_end - start2;
        if span2 < span1 {
            (start2, last_end)
        } else {
            (start1, first_end)
        }
    };

    // 4) Forward pass within the tight window to record optimal positions.
    let mut positions = [0u16; 32];
    let mut qi2 = 0;
    for (ti, &b) in bytes
        .iter()
        .enumerate()
        .take(window_end + 1)
        .skip(window_start)
    {
        if b.to_ascii_lowercase() == query[qi2] as u8 {
            positions[qi2] = ti as u16;
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen as u8))
}

fn subsequence_match_ascii_bytes(query: &[u8], text: &[u8]) -> Option<([u16; 32], u8)> {
    let qlen = query.len();
    let last_qchar = query[qlen - 1];

    let mut qi = 0;
    let mut first_end = 0usize;
    for (ti, &byte) in text.iter().enumerate() {
        if byte.to_ascii_lowercase() == query[qi] {
            qi += 1;
            if qi == qlen {
                first_end = ti;
                break;
            }
        }
    }
    if qi < qlen {
        return None;
    }

    let mut last_end = first_end;
    for (ti, &byte) in text.iter().enumerate().skip(first_end + 1) {
        if byte.to_ascii_lowercase() == last_qchar {
            last_end = ti;
        }
    }

    let (window_start, window_end) = if last_end == first_end {
        (backward_ascii_bytes(text, query, first_end), first_end)
    } else {
        let start1 = backward_ascii_bytes(text, query, first_end);
        let start2 = backward_ascii_bytes(text, query, last_end);
        let span1 = first_end - start1;
        let span2 = last_end - start2;
        if span2 < span1 {
            (start2, last_end)
        } else {
            (start1, first_end)
        }
    };

    let mut positions = [0u16; 32];
    let mut qi2 = 0;
    for (ti, &byte) in text
        .iter()
        .enumerate()
        .take(window_end + 1)
        .skip(window_start)
    {
        if byte.to_ascii_lowercase() == query[qi2] {
            positions[qi2] = ti as u16;
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen as u8))
}

/// Backward scan from `end` (inclusive) to find the tightest window start.
fn backward_ascii(bytes: &[u8], query: &[char], end: usize) -> usize {
    let mut qi = query.len();
    for ti in (0..=end).rev() {
        if bytes[ti].to_ascii_lowercase() == query[qi - 1] as u8 {
            qi -= 1;
            if qi == 0 {
                return ti;
            }
        }
    }
    0 // unreachable if forward pass confirmed the match
}

fn backward_ascii_bytes(bytes: &[u8], query: &[u8], end: usize) -> usize {
    let mut qi = query.len();
    for ti in (0..=end).rev() {
        if bytes[ti].to_ascii_lowercase() == query[qi - 1] {
            qi -= 1;
            if qi == 0 {
                return ti;
            }
        }
    }
    0
}

/// Unicode path — operates on chars.
fn subsequence_match_unicode(query: &[char], text: &str) -> Option<([u16; 32], u8)> {
    let qlen = query.len();
    let last_qchar = query[qlen - 1];

    // 1) Forward pass to confirm match exists and find first endpoint.
    let mut qi = 0;
    let mut first_end = 0usize;
    for (ti, tc) in text.chars().enumerate() {
        if tc.to_lowercase().next() == Some(query[qi]) {
            qi += 1;
            if qi == qlen {
                first_end = ti;
                break;
            }
        }
    }
    if qi < qlen {
        return None;
    }

    // 2) Find last occurrence of the last query char.
    let mut last_end = first_end;
    for (ti, tc) in text.chars().enumerate() {
        if ti > first_end && tc.to_lowercase().next() == Some(last_qchar) {
            last_end = ti;
        }
    }

    // 3) Backward pass from both endpoints; pick tighter window.
    // Collect (char_idx, char) pairs up to max(first_end, last_end) for reverse scanning.
    let max_end = first_end.max(last_end);
    // Use a Vec here since this is the non-ASCII slow path (rare).
    let chars_vec: Vec<(usize, char)> = text.chars().enumerate().take(max_end + 1).collect();

    let start1 = backward_unicode(&chars_vec, query, first_end);
    let (window_start, window_end) = if last_end == first_end {
        (start1, first_end)
    } else {
        let start2 = backward_unicode(&chars_vec, query, last_end);
        let span1 = first_end - start1;
        let span2 = last_end - start2;
        if span2 < span1 {
            (start2, last_end)
        } else {
            (start1, first_end)
        }
    };

    // 4) Forward pass within the tight window to record optimal positions.
    let mut positions = [0u16; 32];
    let mut qi2 = 0;
    for (ti, tc) in text.chars().enumerate() {
        if ti < window_start {
            continue;
        }
        if ti > window_end {
            break;
        }
        if tc.to_lowercase().next() == Some(query[qi2]) {
            positions[qi2] = ti as u16;
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen as u8))
}

/// Backward scan through collected chars to find tightest window start.
fn backward_unicode(chars: &[(usize, char)], query: &[char], end: usize) -> usize {
    let mut qi = query.len();
    for &(ci, ch) in chars.iter().rev() {
        if ci > end {
            continue;
        }
        if ch.to_lowercase().next() == Some(query[qi - 1]) {
            qi -= 1;
            if qi == 0 {
                return ci;
            }
        }
    }
    0
}

fn is_word_boundary_char(c: char) -> bool {
    matches!(c, '/' | '-' | '_' | '.' | ' ' | '\t')
}

fn is_word_boundary_byte(b: u8) -> bool {
    matches!(b, b'/' | b'-' | b'_' | b'.' | b' ' | b'\t')
}

/// Compatibility helper retained for benchmarks.
/// Returns the literal-match tier for a precomputed match window.
pub fn score_match(positions: &[u16; 32], count: u8, text: &str, _pwd_basename: &str) -> i16 {
    let n = count as usize;
    if n == 0 {
        return 0;
    }

    let start = positions[0] as usize;
    for i in 1..n {
        if positions[i] != positions[i - 1] + 1 {
            return 0;
        }
    }

    if start == 0 {
        3
    } else if text
        .chars()
        .nth(start.saturating_sub(1))
        .is_some_and(is_word_boundary_char)
    {
        2
    } else {
        1
    }
}

/// Count occurrences of a byte in a slice.
fn memchr_count(needle: u8, haystack: &[u8]) -> usize {
    haystack.iter().filter(|&&b| b == needle).count()
}

fn history_path() -> PathBuf {
    history_path_for_home(std::env::var_os("HOME").as_deref())
}

fn history_path_for_home(home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(home) = home {
        PathBuf::from(home).join(".local/share/ish/history")
    } else {
        PathBuf::from("/tmp/ish_history")
    }
}

fn cache_path_for(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".bin");
    p.set_file_name(name);
    p
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    p.set_file_name(name);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;

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
    fn history_path_uses_non_utf8_home() {
        let raw = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xf0, 0x80, 0x80, b'h']);
        let path = history_path_for_home(Some(raw.as_os_str()));
        assert_eq!(path, PathBuf::from(raw).join(".local/share/ish/history"));
    }

    #[test]
    fn recency_breaks_ties_within_same_tier() {
        let entries: Vec<String> = (0..100).map(|i| format!("cargo test {i}")).collect();
        let h = History::from_entries(entries);
        let results = h.fuzzy_search("cargo");
        assert_eq!(results[0].entry_idx, 99);
    }

    #[test]
    fn prefix_tier_beats_boundary_substring() {
        let h = History::from_entries(vec!["echo cargo".into(), "cargo build".into()]);
        let results = h.fuzzy_search("cargo");
        assert_eq!(h.get(results[0].entry_idx), "cargo build");
    }

    #[test]
    fn boundary_substring_tier_beats_plain_substring() {
        let h = History::from_entries(vec!["foocargobar".into(), "echo cargo".into()]);
        let results = h.fuzzy_search("cargo");
        assert_eq!(h.get(results[0].entry_idx), "echo cargo");
    }

    #[test]
    fn substring_tier_beats_subsequence_fallback() {
        let h = History::from_entries(vec![
            "git remote add origin https://github.com/joshuarli/smtp-server.git".into(),
            "ls target/debug/".into(),
        ]);
        let results = h.fuzzy_search("target");
        assert_eq!(h.get(results[0].entry_idx), "ls target/debug/");
    }

    #[test]
    fn search_into_sorts_before_limit() {
        let mut entries = vec!["cargo build".to_string()];
        entries.extend((0..220).map(|i| format!("c-x-{i}-a-x-r-x-g-x-o")));
        let h = History::from_entries(entries);
        let mut results = Vec::new();
        h.fuzzy_search_into("cargo", &mut results, 200, "ish");
        assert_eq!(h.get(results[0].entry_idx), "cargo build");
        assert!(results.iter().any(|m| h.get(m.entry_idx) == "cargo build"));
    }

    #[test]
    fn cwd_weight_prefers_ancestor_entries() {
        let mut hist = History::from_entries(vec!["echo cargo".into(), "cargo build".into()]);
        hist.cwds[0] = Some(PathBuf::from("/work/project"));
        hist.cwds[1] = Some(PathBuf::from("/other"));

        let results = hist.fuzzy_search_in_dir("cargo", Path::new("/work/project/src"));
        assert_eq!(hist.get(results[0].entry_idx), "echo cargo");
        assert_eq!(results[0].score, 6);

        let results = hist.fuzzy_search_in_dir("cargo", Path::new("/work/projects"));
        assert_eq!(hist.get(results[0].entry_idx), "cargo build");
    }

    #[test]
    fn cwd_metadata_round_trips_in_text_and_cache() {
        let dir = std::env::temp_dir().join(format!("ish_history_cwd_{}", now_millis()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        let cwd = dir.join("project\twith\\escapes");

        let mut hist = History::load_from_text(&path);
        hist.path = path.clone();
        hist.add_in_dir("echo cwd", Some(&cwd));
        hist.flush_for_read();

        let loaded = History::load_from_text(&path);
        assert_eq!(loaded.cwds[0].as_deref(), Some(cwd.as_path()));

        hist.save_cache();
        let cache = cache_path_for(&path);
        let data = fs::read(&cache).unwrap();
        let parsed = History::parse_cache(&data, &path).expect("v5 cache parse failed");
        assert_eq!(parsed.cwds[0].as_deref(), Some(cwd.as_path()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_history_has_unknown_cwd() {
        let dir = std::env::temp_dir().join(format!("ish_history_legacy_{}", now_millis()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        fs::write(&path, format_history_record(111, 7, "echo legacy")).unwrap();

        let hist = History::load_from_text(&path);
        assert_eq!(hist.get(0), "echo legacy");
        assert_eq!(hist.cwds[0], None);

        let _ = fs::remove_dir_all(dir);
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
        let before = now_millis();
        h.add("new_cmd");
        let after = now_millis();
        let ts = h.timestamp(h.len() - 1);
        assert!(ts >= before && ts <= after);
    }

    #[test]
    fn v4_round_trip() {
        let entries: Vec<String> = vec!["ls -la".into(), "git status".into(), "cargo test".into()];
        let hist = History::from_entries(entries);

        // Serialize to v4 format
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
        buf.extend_from_slice(CACHE_MAGIC_V4);
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(arena_size as u32).to_le_bytes());
        for &ts in &hist.timestamps {
            buf.extend_from_slice(&ts.wrapping_sub(TS_EPOCH_MILLIS).to_le_bytes());
        }
        buf.extend_from_slice(&arena_buf);

        // Parse it back
        let path = PathBuf::from("/dev/null");
        let parsed = History::parse_v4(&buf, &path).expect("v4 round-trip failed");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.get(0), "ls -la");
        assert_eq!(parsed.get(1), "git status");
        assert_eq!(parsed.get(2), "cargo test");
        // Timestamps survive the round-trip
        for i in 0..3 {
            assert_eq!(parsed.timestamp(i), hist.timestamp(i));
        }
    }

    #[test]
    fn structured_log_load_preserves_metadata() {
        let dir = std::env::temp_dir().join(format!("ish_history_meta_{}", now_millis()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", format_history_record(111, 7, "echo one")).unwrap();
        writeln!(file, "{}", format_history_record(222, 8, "echo two")).unwrap();
        writeln!(file, "{}", format_history_record(333, 9, "echo one")).unwrap();

        let hist = History::load_from_text(&path);
        assert_eq!(hist.len(), 2);
        assert_eq!(hist.get(0), "echo two");
        assert_eq!(hist.timestamp(0), 222);
        assert_eq!(hist.session_ids[0], 8);
        assert_eq!(hist.get(1), "echo one");
        assert_eq!(hist.timestamp(1), 333);
        assert_eq!(hist.session_ids[1], 9);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn render_history_file_strips_metadata() {
        let dir = std::env::temp_dir().join(format!("ish_history_render_{}", now_millis()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", format_history_record(111, 7, "echo one")).unwrap();
        writeln!(file, "plain legacy line").unwrap();

        let rendered = render_history_file(&path).unwrap();
        assert_eq!(rendered, "echo one\nplain legacy line\n");

        let _ = fs::remove_dir_all(dir);
    }
}
