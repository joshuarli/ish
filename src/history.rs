use crate::report;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

mod legacy;
mod store;

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_millis() as u64
}

fn new_session_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_nanos() as u64;
    nanos ^ (rustix::process::getpid().as_raw_pid() as u64).wrapping_shl(32)
}

/// Ranking key: (text tier or last-use time for an empty query, directory
/// proximity, usage bonus, usage time, latest occurrence id).
type RankKey = (u64, usize, u32, u64, u32);

#[derive(Clone, Copy, Default)]
struct Usage {
    count: u64,
    timestamp: u64,
}

/// The directory a command last ran in, as a code into `History::directory_paths`.
/// A command that ran in exactly one directory shares the entry's totals, so
/// only repeated directories elsewhere need a record of their own.
const DIRECTORY_NONE: u32 = u32::MAX;
const DIRECTORY_DETAILED: u32 = u32::MAX - 1;

/// What a code of `DIRECTORY_DETAILED` stands for: a command that ran in one
/// directory but also without one, or a command that moved between them.
enum DirectoryDetail {
    Tracked { code: u32, usage: Usage },
    Many(Box<FxHashMap<u32, Usage>>),
}

enum DirectoryCodes<'a> {
    Empty,
    One(std::iter::Once<u32>),
    Many(std::collections::hash_map::Keys<'a, u32, Usage>),
}

impl Iterator for DirectoryCodes<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match self {
            DirectoryCodes::Empty => None,
            DirectoryCodes::One(iter) => iter.next(),
            DirectoryCodes::Many(keys) => keys.next().copied(),
        }
    }
}

impl Usage {
    fn record(&mut self, timestamp: u64) {
        self.count = self.count.saturating_add(1);
        self.timestamp = self.timestamp.max(timestamp);
    }
}

struct EntryUsage {
    latest_id: u32,
    /// ASCII letters present in the command, as one bit per `a`..`z`.
    letters: u32,
    total: Usage,
    directories: u32,
    /// One-based slot in `History::session`, so replacing a command's session
    /// occurrence is a store instead of a search.
    session_position: Option<NonZeroU32>,
}

fn letter_mask(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |mask, &byte| {
        let lower = byte.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            mask | 1 << (lower - b'a')
        } else {
            mask
        }
    })
}

const SESSION_REMOVED: u32 = u32::MAX;

pub struct History {
    /// Unique command text stays packed; updating usage never moves a candidate.
    arena: String,
    offsets: Vec<(u32, u32)>,
    usages: Vec<EntryUsage>,
    index_by_hash: FxHashMap<u64, u32>,
    latest_occurrences: BTreeMap<i64, u32>,
    /// Directory paths addressed by the codes in `EntryUsage::directories`.
    directory_paths: Vec<PathBuf>,
    directory_codes: FxHashMap<PathBuf, u32>,
    /// Entries whose directories are more than one shared code, by entry index.
    directory_details: FxHashMap<u32, DirectoryDetail>,
    /// Startup snapshot plus this shell's commands, oldest first. Other
    /// sessions may update global usage without changing the order or
    /// membership of Up-arrow recall. Replaced occurrences leave a tombstone.
    session: Vec<u32>,
    session_live: usize,
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
        Self::load_from(history_path_for_home(home)?)
    }

    pub fn load_from(path: PathBuf) -> io::Result<Self> {
        let mut history = Self::empty();
        history.store = Some(store::Store::open(&path)?);
        if let Err(error) = history.sync() {
            // A damaged record must not keep the shell from starting. Keep
            // the valid prefix and preserve the rest in a separate file.
            if !store::is_frame_corruption(&error) {
                return Err(error);
            }
            let store = history.store.as_mut().expect("store was just opened");
            let salvage = store.salvage()?;
            report!(
                "ish: history log damaged ({error}); kept {} bytes and moved the rest to {}",
                salvage.kept,
                salvage.moved_to.display()
            );
            history.sync()?;
        }
        let latest = std::mem::take(&mut history.latest_occurrences);
        for &idx in latest.values() {
            history.remember_session(idx as usize);
        }
        history.latest_occurrences = latest;
        history.compact();
        Ok(history)
    }

    fn empty() -> Self {
        Self {
            arena: String::new(),
            offsets: Vec::new(),
            usages: Vec::new(),
            index_by_hash: FxHashMap::default(),
            latest_occurrences: BTreeMap::new(),
            directory_paths: Vec::new(),
            directory_codes: FxHashMap::default(),
            directory_details: FxHashMap::default(),
            session: Vec::new(),
            session_live: 0,
            session_id: new_session_id(),
            generation: 0,
            last_id: 0,
            store: None,
        }
    }

    pub fn storage_path(&self) -> Option<&Path> {
        self.store.as_ref().map(|store| store.path.as_path())
    }

    /// Synchronize a committed snapshot while holding the storage lock, so a
    /// reset cannot mix old occurrences with a new generation.
    pub fn sync(&mut self) -> io::Result<()> {
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        let snapshot = store.snapshot(self.generation, self.last_id)?;
        self.apply_snapshot(snapshot);
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: store::Snapshot) {
        if snapshot.generation != self.generation {
            self.clear();
            self.generation = snapshot.generation;
        }
        if self.arena.is_empty() {
            let text: usize = snapshot
                .occurrences
                .iter()
                .map(|occurrence| occurrence.command.len())
                .sum();
            self.arena.reserve_exact(text);
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
                let start = u32::try_from(self.arena.len()).expect("history text exceeds 4 GiB");
                self.arena.push_str(&occurrence.command);
                let length =
                    u32::try_from(occurrence.command.len()).expect("history command exceeds 4 GiB");
                self.offsets.push((start, length));
                self.usages.push(EntryUsage {
                    latest_id: 0,
                    letters: letter_mask(occurrence.command.as_bytes()),
                    total: Usage::default(),
                    directories: DIRECTORY_NONE,
                    session_position: None,
                });
                self.index_by_hash
                    .insert(hash, u32::try_from(idx).expect("history entries exceed 4 billion"));
                idx
            }
        };
        let directory = occurrence.cwd.map(|cwd| self.intern_directory(cwd));
        if directory.is_none() {
            self.detach_shared_directory(idx);
        } else if let Some(code) = directory {
            self.record_directory(idx, code, occurrence.timestamp);
        }
        self.latest_occurrences
            .remove(&i64::from(self.usages[idx].latest_id));
        self.usages[idx].latest_id =
            u32::try_from(occurrence.id).expect("history occurrence ids exceed 4 billion");
        self.usages[idx].total.record(occurrence.timestamp);
        self.latest_occurrences.insert(
            occurrence.id,
            u32::try_from(idx).expect("history entries exceed 4 billion"),
        );
        self.last_id = self.last_id.max(occurrence.id);
        idx
    }

    fn remember_session(&mut self, idx: usize) {
        if let Some(previous) = self.usages[idx].session_position {
            self.session[previous.get() as usize - 1] = SESSION_REMOVED;
        } else {
            self.session_live += 1;
        }
        self.session.push(
            u32::try_from(idx).expect("history entries exceed 4 billion"),
        );
        self.usages[idx].session_position = NonZeroU32::new(
            u32::try_from(self.session.len()).expect("session exceeds 4 billion entries"),
        );
        self.compact_session();
    }

    /// Reclaim tombstones once they outnumber live occurrences. Compaction is
    /// rare, so rewriting every slot's position stays amortized constant.
    fn compact_session(&mut self) {
        if self.session.len() <= 2 * self.session_live || self.session.len() <= 64 {
            return;
        }
        let session = std::mem::take(&mut self.session);
        self.session = Vec::with_capacity(self.session_live);
        for idx in session {
            if idx == SESSION_REMOVED {
                continue;
            }
            self.session.push(idx);
            self.usages[idx as usize].session_position = NonZeroU32::new(
                u32::try_from(self.session.len()).expect("session exceeds 4 billion entries"),
            );
        }
    }

    /// Give back the growth slack a bulk load leaves behind. Entries arrive in
    /// chunks of doubling capacity, so a loaded history sits up to twice its
    /// content.
    fn compact(&mut self) {
        self.arena.shrink_to_fit();
        self.offsets.shrink_to_fit();
        self.usages.shrink_to_fit();
        self.session.shrink_to_fit();
        self.index_by_hash.shrink_to_fit();
    }

    /// Note that entry `idx` ran in directory `code`. One directory shares the
    /// entry's totals, so recording it costs a code and nothing else. Call this
    /// before the entry's totals take the occurrence.
    fn record_directory(&mut self, idx: usize, code: u32, timestamp: u64) {
        let key = idx as u32;
        let current = self.usages[idx].directories;
        if current == code {
            return;
        }
        if current == DIRECTORY_NONE {
            self.usages[idx].directories = code;
            return;
        }
        if current != DIRECTORY_DETAILED {
            let shared = self.usages[idx].total;
            let mut map = FxHashMap::default();
            map.insert(current, shared);
            self.usages[idx].directories = DIRECTORY_DETAILED;
            self.directory_details
                .insert(key, DirectoryDetail::Many(Box::new(map)));
        }
        if let Some(DirectoryDetail::Tracked { code: tracked, usage }) =
            self.directory_details.get_mut(&key)
            && *tracked == code
        {
            usage.record(timestamp);
            return;
        }
        if let Some(DirectoryDetail::Many(map)) = self.directory_details.get_mut(&key) {
            map.entry(code).or_default().record(timestamp);
            return;
        }
        if let Some(DirectoryDetail::Tracked {
            code: tracked,
            usage,
        }) = self.directory_details.remove(&key)
        {
            let mut map = FxHashMap::default();
            map.insert(tracked, usage);
            map.entry(code).or_default().record(timestamp);
            self.directory_details
                .insert(key, DirectoryDetail::Many(Box::new(map)));
        }
    }

    /// Stop sharing the entry's totals once an occurrence without a directory
    /// arrives: one directory's usage no longer covers every use.
    fn detach_shared_directory(&mut self, idx: usize) {
        let current = self.usages[idx].directories;
        if current == DIRECTORY_NONE || current == DIRECTORY_DETAILED {
            return;
        }
        let usage = self.usages[idx].total;
        self.usages[idx].directories = DIRECTORY_DETAILED;
        self.directory_details
            .insert(idx as u32, DirectoryDetail::Tracked { code: current, usage });
    }

    fn directory_codes_of(&self, idx: usize) -> DirectoryCodes<'_> {
        match self.usages[idx].directories {
            DIRECTORY_NONE => DirectoryCodes::Empty,
            DIRECTORY_DETAILED => match self.directory_details.get(&(idx as u32)) {
                Some(DirectoryDetail::Tracked { code, .. }) => {
                    DirectoryCodes::One(std::iter::once(*code))
                }
                Some(DirectoryDetail::Many(map)) => DirectoryCodes::Many(map.keys()),
                None => DirectoryCodes::Empty,
            },
            code => DirectoryCodes::One(std::iter::once(code)),
        }
    }

    fn intern_directory(&mut self, cwd: PathBuf) -> u32 {
        if let Some(&code) = self.directory_codes.get(cwd.as_path()) {
            return code;
        }
        let code = u32::try_from(self.directory_paths.len()).expect("directory table exhausted");
        self.directory_paths.push(cwd.clone());
        self.directory_codes.insert(cwd, code);
        code
    }

    fn clear(&mut self) {
        self.arena.clear();
        self.offsets.clear();
        self.usages.clear();
        self.index_by_hash.clear();
        self.latest_occurrences.clear();
        self.directory_paths.clear();
        self.directory_codes.clear();
        self.directory_details.clear();
        self.session.clear();
        self.session_live = 0;
        self.last_id = 0;
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
        Self::from_entries_in_dirs(entries.into_iter().map(|command| (command, None)))
    }

    /// Like `from_entries`, recording the directory each command ran in.
    pub fn from_entries_in_dirs(entries: impl IntoIterator<Item = (String, Option<PathBuf>)>) -> Self {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut history = Self::empty();
        history.arena.reserve_exact(
            entries
                .iter()
                .map(|(command, _)| command.len())
                .sum(),
        );
        let timestamp = now_millis();
        for (i, (command, cwd)) in entries.into_iter().enumerate() {
            let id = i as i64 + 1;
            let idx = history.apply_occurrence(store::Occurrence {
                id,
                command,
                timestamp,
                session_id: 0,
                cwd,
            });
            history.remember_session(idx);
        }
        history.compact();
        history
    }

    pub fn add(&mut self, line: &str) -> io::Result<()> {
        let cwd = std::env::current_dir()?;
        self.add_in_dir(line, Some(&cwd))
    }

    /// Persist each use before updating the local search and recall snapshots.
    pub fn add_in_dir(&mut self, line: &str, cwd: Option<&Path>) -> io::Result<()> {
        // Stored verbatim: recalling an entry must run what originally ran,
        // including multi-line input. Display code flattens it instead.
        if line.trim().is_empty() {
            return Ok(());
        }
        let occurrence = store::Occurrence {
            id: self.last_id + 1,
            command: line.to_string(),
            timestamp: now_millis(),
            session_id: self.session_id,
            cwd: cwd.map(Path::to_path_buf),
        };
        if let Some(store) = self.store.as_mut() {
            let (snapshot, _) = store.append(self.generation, self.last_id, occurrence)?;
            self.apply_snapshot(snapshot);
        } else {
            self.apply_occurrence(occurrence);
        }
        let idx = self
            .find_entry_index(hash_str(line), line)
            .expect("committed command missing");
        self.remember_session(idx);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    pub fn timestamp(&self, idx: usize) -> u64 {
        self.usages[idx].total.timestamp
    }
    pub fn frequency(&self, idx: usize) -> u64 {
        self.usages[idx].total.count
    }

    pub fn prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.latest_occurrences
            .values()
            .rev()
            .map(|&idx| self.get(idx as usize))
            .filter(|command| command.starts_with(prefix))
            .nth(skip)
    }

    pub fn session_get(&self, skip: usize) -> Option<&str> {
        self.session
            .iter()
            .rev()
            .filter(|&&idx| idx != SESSION_REMOVED)
            .nth(skip)
            .map(|&idx| self.get(idx as usize))
    }

    pub fn session_prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.session
            .iter()
            .rev()
            .filter(|&&idx| idx != SESSION_REMOVED)
            .map(|&idx| self.get(idx as usize))
            .filter(|command| command.starts_with(prefix))
            .nth(skip)
    }

    /// Unique commands ordered by actual last-use time, with commit IDs breaking ties.
    pub fn command_indices_into(&self, out: &mut Vec<usize>) {
        out.clear();
        out.extend(self.latest_occurrences.values().map(|&idx| idx as usize));
        out.sort_unstable_by(|&a, &b| {
            self.timestamp(a)
                .cmp(&self.timestamp(b))
                .then(self.usages[a].latest_id.cmp(&self.usages[b].latest_id))
        });
    }

    /// All global candidates, independent of the session recall snapshot.
    pub fn search_entry_indices_into(&self, out: &mut Vec<usize>) {
        out.clear();
        out.extend(self.latest_occurrences.values().rev().map(|&idx| idx as usize));
    }

    /// Ctrl+R search: keep the best `limit` matches of `query` among
    /// `candidates`, and report every matching candidate in `matched` so the
    /// next keystroke can refine within it. Text quality precedes directory
    /// context. Bounded usage and age bonuses break ties within that context;
    /// old popularity cannot dominate forever.
    pub fn fuzzy_search_subset_into_in_dir(
        &self,
        query: &str,
        candidates: &[usize],
        matched_indices: &mut Vec<usize>,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: &Path,
    ) {
        self.search_limited(
            query,
            Some(candidates),
            Some(matched_indices),
            results,
            limit,
            Some(cwd),
        );
    }

    /// Convenience over the production search path for callers without a
    /// candidate subset or scratch buffers.
    pub fn fuzzy_search(&self, query: &str) -> Vec<FuzzyMatch> {
        let mut results = Vec::new();
        self.search_limited(query, None, None, &mut results, self.len(), None);
        results
    }

    pub fn fuzzy_search_in_dir(&self, query: &str, cwd: &Path) -> Vec<FuzzyMatch> {
        let mut results = Vec::new();
        self.search_limited(query, None, None, &mut results, self.len(), Some(cwd));
        results
    }

    fn search_limited(
        &self,
        query: &str,
        candidates: Option<&[usize]>,
        mut matched: Option<&mut Vec<usize>>,
        results: &mut Vec<FuzzyMatch>,
        limit: usize,
        cwd: Option<&Path>,
    ) {
        results.clear();
        if let Some(matched) = matched.as_mut() {
            matched.clear();
        }
        let now = now_millis();
        let empty = query.is_empty();
        let mut ranking: Vec<(RankKey, usize)> = Vec::with_capacity(limit.min(self.len()) + 1);
        let mut ascii = [0u8; 32];
        let is_ascii = query.is_ascii() && query.len() <= ascii.len();
        for (slot, byte) in ascii.iter_mut().zip(query.bytes()) {
            *slot = byte.to_ascii_lowercase();
        }
        let mut chars = (!is_ascii).then(|| lowercase_query(query));
        let query_letters = if is_ascii {
            letter_mask(&ascii[..query.len()])
        } else {
            0
        };
        let cwd_depth = cwd.map_or(0, |cwd| cwd.ancestors().count());
        let count = candidates.map_or(self.len(), |candidates| candidates.len());
        for i in 0..count {
            let idx = candidates.map_or(i, |candidates| candidates[i]);
            let text = self.get(idx);
            let rank = if empty {
                MatchRank::Empty
            } else {
                let rank = if is_ascii && text.is_ascii() {
                    if self.usages[idx].letters & query_letters != query_letters {
                        continue;
                    }
                    rank_match_ascii(&ascii[..query.len()], text)
                } else if let Some(chars) = &chars {
                    rank_match(chars, text)
                } else {
                    rank_match(
                        chars.get_or_insert_with(|| lowercase_query(query)),
                        text,
                    )
                };
                let Some(rank) = rank else {
                    continue;
                };
                rank
            };
            if let Some(matched) = matched.as_mut() {
                matched.push(idx);
            }
            if limit == 0 {
                continue;
            }
            let primary = if empty {
                self.timestamp(idx)
            } else {
                rank.score()
            };
            // The text tier alone rejects most candidates once the result
            // list is full, before any directory lookup.
            if ranking.len() == limit && ranking.last().is_some_and(|worst| primary < worst.0.0) {
                continue;
            }
            let key = self.rank_key(idx, primary, empty, cwd, cwd_depth, now);
            if ranking.len() == limit && ranking.last().is_some_and(|worst| key <= worst.0) {
                continue;
            }
            let position = ranking.partition_point(|existing| existing.0 > key);
            if ranking.len() == limit {
                ranking.pop();
            }
            ranking.insert(position, (key, idx));
        }
        if limit == 0 {
            return;
        }
        // Matched positions are only observable for results, so build them
        // once the ranking has chosen the few that fit.
        let bytes = &ascii[..query.len().min(ascii.len())];
        for (_, idx) in ranking {
            let text = self.get(idx);
            let rank = if empty {
                MatchRank::Empty
            } else if is_ascii && text.is_ascii() {
                rank_match_ascii(&ascii[..query.len()], text)
                    .expect("a selected candidate still matches")
            } else {
                rank_match(chars.get_or_insert_with(|| lowercase_query(query)), text)
                    .expect("a selected candidate still matches")
            };
            results.push(materialize_match(rank, idx, text, bytes, chars.as_deref()));
        }
    }

    /// Larger is better. Each candidate's directory score is computed once, so
    /// ordering never repeats the ancestor walk per comparison.
    fn rank_key(
        &self,
        idx: usize,
        primary: u64,
        empty: bool,
        cwd: Option<&Path>,
        cwd_depth: usize,
        now: u64,
    ) -> RankKey {
        let (context, usage) = self.context_usage(idx, cwd, cwd_depth);
        let bonus = if empty { 0 } else { usage_bonus(usage, now) };
        (
            primary,
            context,
            bonus,
            usage.timestamp,
            self.usages[idx].latest_id,
        )
    }

    /// The nearest recorded directory that is `cwd` or an ancestor of it, and
    /// its usage. Walks the smaller side: a command usually ran in one
    /// directory and `cwd` usually nests six deep.
    fn context_usage(&self, idx: usize, cwd: Option<&Path>, cwd_depth: usize) -> (usize, &Usage) {
        let usage = &self.usages[idx];
        let Some(cwd) = cwd else {
            return (0, &usage.total);
        };
        match usage.directories {
            DIRECTORY_NONE => (0, &usage.total),
            DIRECTORY_DETAILED => match self.directory_details.get(&(idx as u32)) {
                Some(DirectoryDetail::Tracked {
                    code,
                    usage: tracked,
                }) => match ancestor_distance(&self.directory_paths[*code as usize], cwd) {
                    Some(distance) => (usize::MAX - distance, tracked),
                    None => (0, &usage.total),
                },
                Some(DirectoryDetail::Many(map)) if map.len() <= cwd_depth => {
                    let mut nearest: Option<(usize, &Usage)> = None;
                    for (code, directory_usage) in map.iter() {
                        let Some(distance) =
                            ancestor_distance(&self.directory_paths[*code as usize], cwd)
                        else {
                            continue;
                        };
                        if nearest.is_none_or(|(closest, _)| distance < closest) {
                            nearest = Some((distance, directory_usage));
                        }
                    }
                    match nearest {
                        Some((distance, directory_usage)) => {
                            (usize::MAX - distance, directory_usage)
                        }
                        None => (0, &usage.total),
                    }
                }
                Some(DirectoryDetail::Many(map)) => {
                    for (distance, directory) in cwd.ancestors().enumerate() {
                        let Some(code) = self.directory_codes.get(directory) else {
                            continue;
                        };
                        if let Some(directory_usage) = map.get(code) {
                            return (usize::MAX - distance, directory_usage);
                        }
                    }
                    (0, &usage.total)
                }
                None => (0, &usage.total),
            },
            code => match ancestor_distance(&self.directory_paths[code as usize], cwd) {
                Some(distance) => (usize::MAX - distance, &usage.total),
                None => (0, &usage.total),
            },
        }
    }

    /// Single-row form of entry `idx` for the pager and `history` output.
    pub fn display(&self, idx: usize) -> std::borrow::Cow<'_, str> {
        display_line(self.get(idx))
    }

    #[cfg(test)]
    fn directory_usage(&self, idx: usize, path: &Path) -> Option<&Usage> {
        let code = self.directory_codes.get(path)?;
        match self.usages[idx].directories {
            DIRECTORY_NONE => None,
            DIRECTORY_DETAILED => match self.directory_details.get(&(idx as u32))? {
                DirectoryDetail::Tracked {
                    code: tracked,
                    usage,
                } => (tracked == code).then_some(usage),
                DirectoryDetail::Many(map) => map.get(code),
            },
            shared => (shared == *code).then_some(&self.usages[idx].total),
        }
    }

    pub fn directories(&self, idx: usize) -> impl Iterator<Item = &Path> {
        let paths = &self.directory_paths;
        self.directory_codes_of(idx)
            .map(move |code| paths[code as usize].as_path())
    }

    pub fn get(&self, idx: usize) -> &str {
        let (start, len) = self.offsets[idx];
        &self.arena[start as usize..start as usize + len as usize]
    }

    fn find_entry_index(&self, hash: u64, text: &str) -> Option<usize> {
        let &idx = self.index_by_hash.get(&hash)?;
        if self.get(idx as usize) == text {
            return Some(idx as usize);
        }
        // Hash collisions must not merge commands. This rare slow path keeps
        // the common case at one lookup without allocating a second text copy.
        (0..self.len()).find(|&idx| self.get(idx) == text)
    }
}

/// How many components `cwd` is below `directory`, when it is below it at
/// all. `Some(0)` means they are the same directory.
fn ancestor_distance(directory: &Path, cwd: &Path) -> Option<usize> {
    if !cwd.as_os_str().as_bytes().starts_with(directory.as_os_str().as_bytes()) {
        return None;
    }
    let rest = cwd.strip_prefix(directory).ok()?;
    Some(rest.components().count())
}

fn usage_bonus(usage: &Usage, now: u64) -> u32 {
    let frequency = usage.count.max(1).ilog2().min(10) * 2;
    let hours = now.saturating_sub(usage.timestamp) / 3_600_000;
    let recency = match hours {
        0..=1 => 24,
        2..=23 => 20,
        24..=167 => 12,
        168..=719 => 4,
        _ => 0,
    };
    frequency + recency
}

pub fn render_history_log(path: &Path) -> io::Result<String> {
    store::render(path)
}

/// History holds typed secrets, so it never falls back to a shared or
/// launch-relative location: without a usable HOME the shell cannot start.
fn history_path_for_home(home: Option<&std::ffi::OsStr>) -> io::Result<PathBuf> {
    match home {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home).join(".local/share/ish/history")),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is unset or empty; cannot locate the history log",
        )),
    }
}

/// A command as one terminal row. History keeps commands verbatim, so
/// multi-line input and control characters are replaced one char for one char
/// (match positions stay valid) only when shown.
pub fn display_line(command: &str) -> std::borrow::Cow<'_, str> {
    if !command.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(command);
    }
    std::borrow::Cow::Owned(
        command
            .chars()
            .map(|c| match c {
                '\n' => '\u{21b5}',
                c if c.is_control() => '\u{fffd}',
                c => c,
            })
            .collect(),
    )
}

/// Lowercase a query using Unicode character semantics.
fn lowercase_query(query: &str) -> Vec<char> {
    query
        .chars()
        .map(|c| c.to_lowercase().next().expect("lowercase scalar missing"))
        .collect()
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

/// Which tier a candidate matched at, and where a contiguous match starts.
/// Ranking needs only this; matched positions are built for the few results
/// that survive the limit.
#[derive(Clone, Copy)]
enum MatchRank {
    Empty,
    Prefix,
    BoundarySubstring(usize),
    Substring(usize),
    Subsequence,
}

impl MatchRank {
    fn score(self) -> u64 {
        match self {
            Self::Empty | Self::Subsequence => 0,
            Self::Substring(_) => 1,
            Self::BoundarySubstring(_) => 2,
            Self::Prefix => 3,
        }
    }
}

fn rank_match(query: &[char], text: &str) -> Option<MatchRank> {
    if starts_with_icase(query, text) {
        return Some(MatchRank::Prefix);
    }

    if let Some(start) = find_substring_icase(query, text, true) {
        return Some(MatchRank::BoundarySubstring(start));
    }

    if let Some(start) = find_substring_icase(query, text, false) {
        return Some(MatchRank::Substring(start));
    }

    subsequence_match(query, text).map(|_| MatchRank::Subsequence)
}

fn rank_match_ascii(query: &[u8], text: &str) -> Option<MatchRank> {
    if starts_with_icase_ascii(query, text.as_bytes()) {
        return Some(MatchRank::Prefix);
    }

    // Every substring is also a subsequence, so entries that fail this
    // single-pass check skip both substring scans and the window search.
    // Most entries fail it while the query is still being typed.
    if !is_subsequence_ascii_bytes(query, text.as_bytes()) {
        return None;
    }

    if let Some(start) = find_substring_icase_ascii_bytes(query, text.as_bytes(), true) {
        return Some(MatchRank::BoundarySubstring(start));
    }

    if let Some(start) = find_substring_icase_ascii_bytes(query, text.as_bytes(), false) {
        return Some(MatchRank::Substring(start));
    }

    Some(MatchRank::Subsequence)
}

fn materialize_match(
    rank: MatchRank,
    entry_idx: usize,
    text: &str,
    bytes: &[u8],
    chars: Option<&[char]>,
) -> FuzzyMatch {
    let len = chars.map_or(bytes.len(), |chars| chars.len());
    match rank {
        MatchRank::Empty => contiguous_match(entry_idx, 0, 0, 0),
        MatchRank::Prefix => contiguous_match(entry_idx, 3, 0, len),
        MatchRank::BoundarySubstring(start) => contiguous_match(entry_idx, 2, start, len),
        MatchRank::Substring(start) => contiguous_match(entry_idx, 1, start, len),
        MatchRank::Subsequence => {
            let (positions, count) = chars.map_or_else(
                || subsequence_match_ascii_bytes(bytes, text.as_bytes()),
                |query| subsequence_match(query, text),
            )
            .expect("a ranked subsequence still matches");
            FuzzyMatch {
                entry_idx,
                match_positions: positions,
                match_count: count,
                score: 0,
            }
        }
    }
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
            if qi2 < positions.len() {
                positions[qi2] = ti;
            }
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
    let (words, remainder) = haystack.as_chunks::<8>();
    for (index, chunk) in words.iter().enumerate() {
        let word = u64::from_le_bytes(*chunk);
        let matches = zero_byte_mask(word ^ first_word) | zero_byte_mask(word ^ second_word);
        if matches != 0 {
            return Some(index * 8 + matches.trailing_zeros() as usize / 8);
        }
    }
    let offset = haystack.len() - remainder.len();
    remainder
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
            if qi2 < positions.len() {
                positions[qi2] = ti;
            }
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
            if qi2 < positions.len() {
                positions[qi2] = ti;
            }
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
    fn single_directory_usage_shares_command_totals() {
        let dir = PathBuf::from("/work/a");
        let history = History::from_entries_in_dirs([
            ("cargo build".to_string(), Some(dir.clone())),
            ("cargo build".to_string(), Some(dir.clone())),
        ]);
        assert_eq!(history.frequency(0), 2);
        assert_eq!(history.directory_usage(0, &dir).unwrap().count, 2);
        assert_eq!(history.directory_usage(0, &dir).unwrap().timestamp, history.timestamp(0));
    }

    #[test]
    fn occurrence_without_directory_detaches_shared_totals() {
        let dir = PathBuf::from("/work/a");
        let history = History::from_entries_in_dirs([
            ("cargo build".to_string(), Some(dir.clone())),
            ("cargo build".to_string(), None),
        ]);
        assert_eq!(history.frequency(0), 2);
        assert_eq!(history.directory_usage(0, &dir).unwrap().count, 1);
    }

    #[test]
    fn moving_between_directories_keeps_each_usage() {
        let a = PathBuf::from("/work/a");
        let b = PathBuf::from("/work/b");
        let history = History::from_entries_in_dirs([
            ("cargo build".to_string(), Some(a.clone())),
            ("cargo build".to_string(), Some(b.clone())),
            ("cargo build".to_string(), Some(a.clone())),
        ]);
        assert_eq!(history.frequency(0), 3);
        assert_eq!(history.directory_usage(0, &a).unwrap().count, 2);
        assert_eq!(history.directory_usage(0, &b).unwrap().count, 1);
    }

    #[test]
    fn session_recall_skips_replaced_occurrences() {
        let history = History::from_entries(vec![
            "echo a".to_string(),
            "echo b".to_string(),
            "echo a".to_string(),
            "echo c".to_string(),
        ]);
        assert_eq!(history.session_get(0), Some("echo c"));
        assert_eq!(history.session_get(1), Some("echo a"));
        assert_eq!(history.session_get(2), Some("echo b"));
        assert_eq!(history.session_get(3), None);
    }

    #[test]
    fn session_compaction_keeps_recall_order() {
        let mut entries: Vec<String> = (0..200).map(|i| format!("echo {i}")).collect();
        entries.extend((0..300).map(|_| "echo repeated".to_string()));
        let history = History::from_entries(entries);
        assert_eq!(history.session_get(0), Some("echo repeated"));
        assert_eq!(history.session_get(1), Some("echo 199"));
        assert_eq!(history.session_get(2), Some("echo 198"));
        assert!(history.session.len() <= 2 * history.session_live.max(1) + 64);
    }

    #[test]
    fn directories_share_one_path_across_commands() {
        let same = PathBuf::from("/work/shared");
        let history = History::from_entries_in_dirs([
            ("one".to_string(), Some(same.clone())),
            ("two".to_string(), Some(same.clone())),
            ("three".to_string(), Some(PathBuf::from("/work/other"))),
        ]);
        assert_eq!(history.directory_paths.len(), 2);
        assert_eq!(history.directories(0).collect::<Vec<_>>(), [same.as_path()]);
        assert_eq!(history.directories(2).collect::<Vec<_>>(), [Path::new("/work/other")]);
    }

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
                let text: Vec<u8> = (0..width)
                    .map(|_| alphabet[next() % alphabet.len()])
                    .collect();
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
                let text: Vec<u8> = (0..width)
                    .map(|_| alphabet[next() % alphabet.len()])
                    .collect();
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
    fn subsequence_no_match() {
        let q: Vec<char> = "xyz".chars().collect();
        assert!(subsequence_match(&q, "hello").is_none());
    }

    #[test]
    fn history_path_uses_non_utf8_home() {
        let raw = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xf0, 0x80, 0x80, b'h']);
        let path = history_path_for_home(Some(raw.as_os_str())).unwrap();
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
        let mut candidates = Vec::new();
        h.search_entry_indices_into(&mut candidates);
        h.fuzzy_search_subset_into_in_dir(
            "cargo",
            &candidates,
            &mut Vec::new(),
            &mut results,
            200,
            Path::new("ish"),
        );
        assert_eq!(h.get(results[0].entry_idx), "cargo build");
        assert!(results.iter().any(|m| h.get(m.entry_idx) == "cargo build"));
    }
}

#[cfg(test)]
mod persistence_tests;
