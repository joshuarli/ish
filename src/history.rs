use fxhash::FxHashMap;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

mod legacy;
mod store;

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch").as_millis() as u64
}

fn new_session_id() -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch").as_nanos() as u64;
    nanos ^ (rustix::process::getpid().as_raw_pid() as u64).wrapping_shl(32)
}

#[derive(Default)]
struct Usage {
    count: u64,
    timestamp: u64,
}

struct EntryUsage {
    latest_id: i64,
    total: Usage,
    directories: FxHashMap<PathBuf, Usage>,
    session_occurrence_id: Option<i64>,
}

pub struct History {
    /// Unique command text stays packed; updating usage never moves a candidate.
    arena: String,
    offsets: Vec<(usize, usize)>,
    usages: Vec<EntryUsage>,
    index_by_hash: FxHashMap<u64, usize>,
    latest_occurrences: BTreeMap<i64, usize>,
    /// Startup snapshot plus this shell's commands. Other sessions may update
    /// global usage without changing the order or membership of Up-arrow recall.
    session: BTreeMap<i64, usize>,
    session_id: u64,
    generation: i64,
    last_id: i64,
    store: Option<store::Store>,
}

impl History {
    pub fn load() -> io::Result<Self> {
        Self::load_from_home(std::env::var_os("HOME").as_deref())
    }

    pub fn load_from_home(home: Option<&std::ffi::OsStr>) -> io::Result<Self> {
        Self::load_from(history_path_for_home(home))
    }

    pub fn load_from(path: PathBuf) -> io::Result<Self> {
        let mut history = Self::empty();
        history.store = Some(store::Store::open(&path)?);
        history.sync()?;
        for (&id, &idx) in &history.latest_occurrences {
            history.session.insert(id, idx);
            history.usages[idx].session_occurrence_id = Some(id);
        }
        Ok(history)
    }

    fn empty() -> Self {
        Self { arena: String::new(), offsets: Vec::new(), usages: Vec::new(),
            index_by_hash: FxHashMap::default(), latest_occurrences: BTreeMap::new(),
            session: BTreeMap::new(), session_id: new_session_id(), generation: 0,
            last_id: 0, store: None }
    }

    pub fn database_path(&self) -> Option<&Path> {
        self.store.as_ref().map(|store| store.path.as_path())
    }

    /// Synchronize a committed snapshot. Generation and rows are read in the
    /// same transaction, so reset cannot mix old rows with a new generation.
    pub fn sync(&mut self) -> io::Result<()> {
        let Some(store) = self.store.as_mut() else { return Ok(()); };
        let snapshot = store.snapshot(self.generation, self.last_id)?;
        self.apply_snapshot(snapshot);
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: store::Snapshot) {
        if snapshot.generation != self.generation {
            self.clear();
            self.generation = snapshot.generation;
        }
        for occurrence in snapshot.occurrences {
            self.apply_occurrence(occurrence);
        }
    }

    fn apply_occurrence(&mut self, occurrence: store::Occurrence) -> usize {
        let hash = hash_str(&occurrence.command);
        let idx = match self.find_entry_index(hash, &occurrence.command) {
            Some(idx) => idx,
            None => {
                let idx = self.offsets.len();
                let start = self.arena.len();
                self.arena.push_str(&occurrence.command);
                self.offsets.push((start, occurrence.command.len()));
                self.usages.push(EntryUsage { latest_id: 0, total: Usage::default(),
                    directories: FxHashMap::default(), session_occurrence_id: None });
                self.index_by_hash.insert(hash, idx);
                idx
            }
        };
        let usage = &mut self.usages[idx];
        self.latest_occurrences.remove(&usage.latest_id);
        usage.latest_id = occurrence.id;
        usage.total.count = usage.total.count.saturating_add(1);
        usage.total.timestamp = usage.total.timestamp.max(occurrence.timestamp);
        if let Some(cwd) = occurrence.cwd {
            let directory = usage.directories.entry(cwd).or_default();
            directory.count = directory.count.saturating_add(1);
            directory.timestamp = directory.timestamp.max(occurrence.timestamp);
        }
        self.latest_occurrences.insert(occurrence.id, idx);
        self.last_id = self.last_id.max(occurrence.id);
        idx
    }

    fn remember_session(&mut self, idx: usize, id: i64) {
        if let Some(previous) = self.usages[idx].session_occurrence_id {
            self.session.remove(&previous);
        }
        self.usages[idx].session_occurrence_id = Some(id);
        self.session.insert(id, idx);
    }

    fn clear(&mut self) {
        self.arena.clear();
        self.offsets.clear();
        self.usages.clear();
        self.index_by_hash.clear();
        self.latest_occurrences.clear();
        self.session.clear();
        self.last_id = 0;
    }

    pub fn compact(&mut self) -> io::Result<()> {
        if let Some(store) = self.store.as_mut() {
            store.compact()?;
        }
        self.sync()
    }

    pub fn reset(&mut self) -> io::Result<()> {
        let generation = match self.store.as_mut() {
            Some(store) => store.reset()?,
            None => self.generation + 1,
        };
        self.clear();
        self.generation = generation;
        Ok(())
    }

    /// Create an isolated history without filesystem access for tests and benchmarks.
    pub fn from_entries(entries: Vec<String>) -> Self {
        let mut history = Self::empty();
        let timestamp = now_millis();
        for (i, command) in entries.into_iter().enumerate() {
            let id = i as i64 + 1;
            let idx = history.apply_occurrence(store::Occurrence { id, command, timestamp,
                session_id: 0, cwd: None });
            history.remember_session(idx, id);
        }
        history
    }

    pub fn add(&mut self, line: &str) -> io::Result<()> {
        let cwd = std::env::current_dir()?;
        self.add_in_dir(line, Some(&cwd))
    }

    /// Persist each use before updating the local search and recall snapshots.
    pub fn add_in_dir(&mut self, line: &str, cwd: Option<&Path>) -> io::Result<()> {
        let line = line.trim().replace('\n', " ");
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let occurrence = store::Occurrence { id: self.last_id + 1, command: line.to_string(),
            timestamp: now_millis(), session_id: self.session_id, cwd: cwd.map(Path::to_path_buf) };
        let id = if let Some(store) = self.store.as_mut() {
            let (snapshot, id) = store.append(self.generation, self.last_id, occurrence)?;
            self.apply_snapshot(snapshot);
            id
        } else {
            let id = occurrence.id;
            self.apply_occurrence(occurrence);
            id
        };
        let idx = self.find_entry_index(hash_str(line), line).expect("committed command missing");
        self.remember_session(idx, id);
        Ok(())
    }

    pub fn len(&self) -> usize { self.offsets.len() }
    pub fn is_empty(&self) -> bool { self.offsets.is_empty() }
    pub fn timestamp(&self, idx: usize) -> u64 { self.usages[idx].total.timestamp }
    pub fn frequency(&self, idx: usize) -> u64 { self.usages[idx].total.count }

    pub fn prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.latest_occurrences.values().rev().map(|&idx| self.get(idx))
            .filter(|command| command.starts_with(prefix)).nth(skip)
    }

    pub fn session_get(&self, skip: usize) -> Option<&str> {
        self.session.values().rev().nth(skip).map(|&idx| self.get(idx))
    }

    pub fn session_prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.session.values().rev().map(|&idx| self.get(idx))
            .filter(|command| command.starts_with(prefix)).nth(skip)
    }

    /// Unique commands ordered by actual last-use time, with commit IDs breaking ties.
    pub fn command_indices_into(&self, out: &mut Vec<usize>) {
        out.clear();
        out.extend(self.latest_occurrences.values().copied());
        out.sort_unstable_by(|&a, &b| self.timestamp(a).cmp(&self.timestamp(b))
            .then(self.usages[a].latest_id.cmp(&self.usages[b].latest_id)));
    }

    /// All global candidates, independent of the session recall snapshot.
    pub fn search_entry_indices_into(&self, out: &mut Vec<usize>) {
        out.clear();
        out.extend(self.latest_occurrences.values().rev().copied());
    }

    /// Text quality precedes directory context. Bounded usage and age bonuses
    /// break ties within that context; old popularity cannot dominate forever.
    pub fn fuzzy_search(&self, query: &str) -> Vec<FuzzyMatch> {
        self.fuzzy_search_scored(query, "")
    }

    pub fn fuzzy_search_scored(&self, query: &str, cwd: &str) -> Vec<FuzzyMatch> {
        self.search_all(query, (!cwd.is_empty()).then(|| Path::new(cwd)))
    }

    pub fn fuzzy_search_in_dir(&self, query: &str, cwd: &Path) -> Vec<FuzzyMatch> {
        self.search_all(query, Some(cwd))
    }

    fn search_all(&self, query: &str, cwd: Option<&Path>) -> Vec<FuzzyMatch> {
        let mut results = Vec::new();
        let matcher = PreparedQuery::new(query);
        for idx in 0..self.len() {
            if let Some(result) = matcher.classify(self.get(idx), idx) {
                results.push(result);
            }
        }
        let now = now_millis();
        results.sort_unstable_by(|a, b| self.compare(a, b, cwd, query.is_empty(), now));
        results
    }

    pub fn fuzzy_search_into(&self, query: &str, results: &mut Vec<FuzzyMatch>, limit: usize, cwd: &str) {
        self.search_limited(query, None, None, results, limit,
            (!cwd.is_empty()).then(|| Path::new(cwd)));
    }

    pub fn fuzzy_search_into_in_dir(&self, query: &str, results: &mut Vec<FuzzyMatch>, limit: usize, cwd: &Path) {
        self.search_limited(query, None, None, results, limit, Some(cwd));
    }

    pub fn fuzzy_search_subset_into(&self, query: &str, candidates: &[usize], matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>, limit: usize) {
        self.search_limited(query, Some(candidates), Some(matched_indices), results, limit, None);
    }

    pub fn fuzzy_search_subset_into_in_dir(&self, query: &str, candidates: &[usize], matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>, limit: usize, cwd: &Path) {
        self.search_limited(query, Some(candidates), Some(matched_indices), results, limit, Some(cwd));
    }

    fn search_limited(&self, query: &str, candidates: Option<&[usize]>, mut matched: Option<&mut Vec<usize>>,
        results: &mut Vec<FuzzyMatch>, limit: usize, cwd: Option<&Path>) {
        results.clear();
        if let Some(matched) = matched.as_mut() { matched.clear(); }
        let now = now_millis();
        let mut ascii = [0u8; 32];
        let is_ascii = query.is_ascii() && query.len() <= ascii.len();
        for (slot, byte) in ascii.iter_mut().zip(query.bytes()) { *slot = byte.to_ascii_lowercase(); }
        let mut chars = (!is_ascii).then(|| lowercase_query(query));
        let count = candidates.map_or(self.len(), |candidates| candidates.len());
        for i in 0..count {
            let idx = candidates.map_or(i, |candidates| candidates[i]);
            let text = self.get(idx);
            let result = if query.is_empty() {
                Some(contiguous_match(idx, 0, 0, 0))
            } else if is_ascii && text.is_ascii() {
                classify_match_ascii(&ascii[..query.len()], text, idx)
            } else if let Some(chars) = &chars {
                classify_match(chars, text, idx)
            } else {
                classify_match(chars.get_or_insert_with(|| lowercase_query(query)), text, idx)
            };
            let Some(result) = result else { continue; };
            if let Some(matched) = matched.as_mut() { matched.push(idx); }
            if limit == 0 { continue; }
            let position = results.binary_search_by(|existing| self.compare(existing, &result, cwd, query.is_empty(), now))
                .unwrap_or_else(|position| position);
            if position < limit {
                results.insert(position, result);
                if results.len() > limit { results.pop(); }
            }
        }
    }

    fn context_usage(&self, idx: usize, cwd: Option<&Path>) -> (usize, &Usage) {
        let usage = &self.usages[idx];
        let Some(cwd) = cwd else { return (0, &usage.total); };
        for (distance, directory) in cwd.ancestors().enumerate() {
            if let Some(directory_usage) = usage.directories.get(directory) {
                return (usize::MAX - distance, directory_usage);
            }
        }
        (0, &usage.total)
    }

    fn compare(&self, a: &FuzzyMatch, b: &FuzzyMatch, cwd: Option<&Path>, empty: bool, now: u64)
        -> std::cmp::Ordering {
        if empty {
            let timestamps = self.timestamp(b.entry_idx).cmp(&self.timestamp(a.entry_idx));
            if !timestamps.is_eq() { return timestamps; }
        } else {
            let tiers = b.score.cmp(&a.score);
            if !tiers.is_eq() { return tiers; }
        }
        let (a_context, a_usage) = self.context_usage(a.entry_idx, cwd);
        let (b_context, b_usage) = self.context_usage(b.entry_idx, cwd);
        b_context.cmp(&a_context)
            .then_with(|| if empty { std::cmp::Ordering::Equal }
                else { usage_bonus(b_usage, now).cmp(&usage_bonus(a_usage, now)) })
            .then_with(|| b_usage.timestamp.cmp(&a_usage.timestamp))
            .then_with(|| self.usages[b.entry_idx].latest_id.cmp(&self.usages[a.entry_idx].latest_id))
    }

    pub fn get(&self, idx: usize) -> &str {
        let (start, len) = self.offsets[idx];
        &self.arena[start..start+len]
    }

    fn find_entry_index(&self, hash: u64, text: &str) -> Option<usize> {
        let &idx = self.index_by_hash.get(&hash)?;
        if self.get(idx) == text { return Some(idx); }
        // Hash collisions must not merge commands. This rare slow path keeps
        // the common case at one lookup without allocating a second text copy.
        (0..self.len()).find(|&idx| self.get(idx) == text)
    }
}

fn usage_bonus(usage: &Usage, now: u64) -> u32 {
    let frequency = usage.count.max(1).ilog2().min(10) * 2;
    let hours = now.saturating_sub(usage.timestamp) / 3_600_000;
    let recency = match hours { 0..=1 => 24, 2..=23 => 20, 24..=167 => 12,
        168..=719 => 4, _ => 0 };
    frequency + recency
}

pub fn render_history_database(path: &Path) -> io::Result<String> {
    store::render(path)
}

fn history_path_for_home(home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(home) = home { PathBuf::from(home).join(".local/share/ish/history") }
    else { PathBuf::from("/tmp/ish_history") }
}

/// Lowercase a query using Unicode character semantics.
fn lowercase_query(query: &str) -> Vec<char> {
    query.chars().map(|c| c.to_lowercase().next().expect("lowercase scalar missing")).collect()
}

#[derive(Debug)]
pub struct FuzzyMatch {
    pub entry_idx: usize,
    /// Matched character indices, independent of UTF-8 byte lengths.
    pub match_positions: [usize; 32],
    pub match_count: u8,
    /// Match tier. Higher = stronger literal match.
    /// 3 = prefix, 2 = boundary substring, 1 = substring, 0 = subsequence fallback.
    pub score: i16,
}

/// A lowercased query prepared once per search. ASCII queries also keep their
/// bytes, so ASCII entries (nearly all history) are classified without char
/// decoding. Entries with non-ASCII text keep the char path: it compares
/// Unicode-lowercased chars and reports char positions, which byte matching
/// would change.
struct PreparedQuery {
    chars: Vec<char>,
    ascii: Option<([u8; 32], usize)>,
}

impl PreparedQuery {
    fn new(query: &str) -> Self {
        let chars = lowercase_query(query);
        let ascii = (query.is_ascii() && query.len() <= 32).then(|| {
            let mut bytes = [0u8; 32];
            for (slot, byte) in bytes.iter_mut().zip(query.bytes()) {
                *slot = byte.to_ascii_lowercase();
            }
            (bytes, query.len())
        });
        Self { chars, ascii }
    }

    fn classify(&self, text: &str, entry_idx: usize) -> Option<FuzzyMatch> {
        if self.chars.is_empty() { return Some(contiguous_match(entry_idx, 0, 0, 0)); }
        match &self.ascii {
            Some((bytes, len)) if text.is_ascii() => {
                classify_match_ascii(&bytes[..*len], text, entry_idx)
            }
            _ => classify_match(&self.chars, text, entry_idx),
        }
    }
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

    // Every substring is also a subsequence, so entries that fail this
    // single-pass check skip both substring scans and the window search.
    // Most entries fail it while the query is still being typed.
    if !is_subsequence_ascii_bytes(query, text.as_bytes()) {
        return None;
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
    let mut positions = [0usize; 32];
    let count = len.min(positions.len()).min(u8::MAX as usize);
    for (offset, slot) in positions.iter_mut().take(count).enumerate() {
        *slot = start + offset;
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

    // `query` is ASCII-lowercased, so a start can only begin at its first byte
    // in either case. Jump between those instead of testing every start.
    let first = query[0];
    let first_upper = first.to_ascii_uppercase();
    let last_start = text.len() - query.len();
    let mut start = 0;
    while start <= last_start {
        start += find_either_byte(&text[start..=last_start], first, first_upper)?;
        let at_boundary = start == 0 || is_word_boundary_byte(text[start - 1]);
        if (at_boundary || !boundary_only)
            && text[start + 1..start + query.len()]
                .iter()
                .zip(&query[1..])
                .all(|(&byte, &query_byte)| byte.to_ascii_lowercase() == query_byte)
        {
            return Some(start);
        }
        start += 1;
    }

    None
}

/// Check if `query` chars appear in `text` in order (case-insensitive).
/// Uses a forward-then-backward scan to find the tightest match window,
/// then a final forward pass within that window for optimal positions.
/// Returns a fixed-size array of matched character indices and the count.
/// The ASCII path uses stack arrays without heap allocations.
pub fn subsequence_match(query: &[char], text: &str) -> Option<([usize; 32], u8)> {
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
fn subsequence_match_ascii(query: &[char], text: &str) -> Option<([usize; 32], u8)> {
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
    let mut positions = [0usize; 32];
    let mut qi2 = 0;
    for (ti, &b) in bytes
        .iter()
        .enumerate()
        .take(window_end + 1)
        .skip(window_start)
    {
        if b.to_ascii_lowercase() == query[qi2] as u8 {
            if qi2 < positions.len() { positions[qi2] = ti; }
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen.min(32) as u8))
}

const BYTE_WORD_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_WORD_HIGHS: u64 = 0x8080_8080_8080_8080;

#[inline]
fn zero_byte_mask(word: u64) -> u64 {
    word.wrapping_sub(BYTE_WORD_ONES) & !word & BYTE_WORD_HIGHS
}

/// Index of the first byte equal to `first` or `second`, eight bytes at a time.
/// History entries are short, where this beats a libc `memchr` call per byte
/// of the query.
#[inline]
fn find_either_byte(haystack: &[u8], first: u8, second: u8) -> Option<usize> {
    let first_word = u64::from(first) * BYTE_WORD_ONES;
    let second_word = u64::from(second) * BYTE_WORD_ONES;
    let mut words = haystack.chunks_exact(8);
    for (index, chunk) in (&mut words).enumerate() {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        let matches = zero_byte_mask(word ^ first_word) | zero_byte_mask(word ^ second_word);
        if matches != 0 {
            return Some(index * 8 + matches.trailing_zeros() as usize / 8);
        }
    }
    let offset = haystack.len() - words.remainder().len();
    words
        .remainder()
        .iter()
        .position(|&byte| byte == first || byte == second)
        .map(|index| offset + index)
}

/// Whether `query` (already ASCII-lowercased) is a case-insensitive
/// subsequence of `text`. Agrees with `subsequence_match_ascii_bytes` returning
/// `Some`, without computing positions.
fn is_subsequence_ascii_bytes(query: &[u8], mut text: &[u8]) -> bool {
    for &byte in query {
        let uppercase = byte.to_ascii_uppercase();
        // Query bytes usually continue a run in the text, so test the next
        // byte before setting up a word scan.
        if let Some((&next, rest)) = text.split_first()
            && (next == byte || next == uppercase)
        {
            text = rest;
            continue;
        }
        let Some(position) = find_either_byte(text, byte, uppercase) else {
            return false;
        };
        text = &text[position + 1..];
    }
    true
}

fn subsequence_match_ascii_bytes(query: &[u8], text: &[u8]) -> Option<([usize; 32], u8)> {
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

    let mut positions = [0usize; 32];
    let mut qi2 = 0;
    for (ti, &byte) in text
        .iter()
        .enumerate()
        .take(window_end + 1)
        .skip(window_start)
    {
        if byte.to_ascii_lowercase() == query[qi2] {
            if qi2 < positions.len() { positions[qi2] = ti; }
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen.min(32) as u8))
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
fn subsequence_match_unicode(query: &[char], text: &str) -> Option<([usize; 32], u8)> {
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
    let mut positions = [0usize; 32];
    let mut qi2 = 0;
    for (ti, tc) in text.chars().enumerate() {
        if ti < window_start {
            continue;
        }
        if ti > window_end {
            break;
        }
        if tc.to_lowercase().next() == Some(query[qi2]) {
            if qi2 < positions.len() { positions[qi2] = ti; }
            qi2 += 1;
            if qi2 == qlen {
                break;
            }
        }
    }

    Some((positions, qlen.min(32) as u8))
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
pub fn score_match(positions: &[usize; 32], count: u8, text: &str, _pwd_basename: &str) -> i16 {
    let n = count as usize;
    if n == 0 {
        return 0;
    }

    let start = positions[0];
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


#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn subsequence() {
        let q: Vec<char> = "gco".chars().collect();
        let (positions, count) = subsequence_match(&q, "git checkout").unwrap();
        assert_eq!(count, 3);
        assert_eq!(&positions[..3], &[0, 4, 9]);
    }

    #[test]
    fn subsequence_prefilter_agrees_with_window_search_across_word_boundaries() {
        // Alphabet mixes cased letters, digits, separators, and non-ASCII bytes
        // so both case arms and the word/remainder split are exercised.
        let alphabet = "abcAB01 -/é".as_bytes();
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };
        for width in 0..=40 {
            for _ in 0..200 {
                let text: Vec<u8> = (0..width).map(|_| alphabet[next() % alphabet.len()]).collect();
                // Queries are lowercased ASCII, as the byte search path requires.
                let query: Vec<u8> = (0..1 + next() % 5)
                    .map(|_| alphabet[next() % 8].to_ascii_lowercase())
                    .collect();
                assert_eq!(
                    is_subsequence_ascii_bytes(&query, &text),
                    subsequence_match_ascii_bytes(&query, &text).is_some(),
                    "query {query:?} text {text:?}",
                );
            }
        }
    }

    #[test]
    fn substring_scan_agrees_with_testing_every_start() {
        fn every_start(query: &[u8], text: &[u8], boundary_only: bool) -> Option<usize> {
            if query.len() > text.len() {
                return None;
            }
            (0..=text.len() - query.len()).find(|&start| {
                (!boundary_only || start == 0 || is_word_boundary_byte(text[start - 1]))
                    && query
                        .iter()
                        .enumerate()
                        .all(|(offset, &byte)| text[start + offset].to_ascii_lowercase() == byte)
            })
        }
        let alphabet = b"abAB0 -/_.";
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };
        for width in 0..=40 {
            for _ in 0..300 {
                let text: Vec<u8> = (0..width).map(|_| alphabet[next() % alphabet.len()]).collect();
                let query: Vec<u8> = (0..1 + next() % 4)
                    .map(|_| alphabet[next() % alphabet.len()].to_ascii_lowercase())
                    .collect();
                for boundary_only in [false, true] {
                    assert_eq!(
                        find_substring_icase_ascii_bytes(&query, &text, boundary_only),
                        every_start(&query, &text, boundary_only),
                        "query {query:?} text {text:?} boundary {boundary_only}",
                    );
                }
            }
        }
    }

    #[test]
    fn prepared_query_classifies_like_the_char_path() {
        let ascii = b"abcAB0 -/_.";
        let mut state = 0xd1b5_4a32_d192_ed03_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };
        let check = |query: &str, text: &str| {
            let chars = lowercase_query(query);
            let expected = classify_match(&chars, text, 7);
            let actual = PreparedQuery::new(query).classify(text, 7);
            assert_eq!(
                actual.map(|m| (m.entry_idx, m.score, m.match_positions, m.match_count)),
                expected.map(|m| (m.entry_idx, m.score, m.match_positions, m.match_count)),
                "query {query:?} text {text:?}",
            );
        };
        for width in 0..=40 {
            for _ in 0..200 {
                let text: String = (0..width).map(|_| ascii[next() % ascii.len()] as char).collect();
                let query: String = (0..1 + next() % 5).map(|_| ascii[next() % 8] as char).collect();
                check(&query, &text);
            }
        }
        // Non-ASCII entries must keep char semantics: Kelvin sign lowercases to
        // `k`, and positions count chars rather than bytes.
        check("k", "\u{212a}elvin");
        check("ca", "é cafe");
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

}

#[cfg(test)]
mod persistence_tests;
