use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub struct History {
    /// All entry text packed into a single allocation.
    arena: String,
    /// (start, len) byte offsets into `arena` for each entry.
    offsets: Vec<(u32, u16)>,
    /// Hash index for O(1) duplicate checks in add().
    hashes: HashSet<u64>,
    path: PathBuf,
}

impl History {
    pub fn load() -> Self {
        let path = history_path();
        let (arena, offsets, hashes) = match fs::read(&path) {
            Ok(data) => {
                let line_count = memchr_count(b'\n', &data);
                let mut seen = HashSet::with_capacity(line_count);
                let mut deduped: Vec<&str> = Vec::with_capacity(line_count);
                let mut hashes_vec: Vec<u64> = Vec::with_capacity(line_count);

                // rsplit iterates from end — first insert of each hash wins (= most recent)
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

                // Pack into a single arena allocation
                let total: usize = deduped.iter().map(|s| s.len()).sum();
                let mut arena = String::with_capacity(total);
                let mut offsets = Vec::with_capacity(deduped.len());
                for line in &deduped {
                    let start = arena.len() as u32;
                    arena.push_str(line);
                    offsets.push((start, line.len() as u16));
                }

                (arena, offsets, hashes_vec.into_iter().collect())
            }
            Err(_) => (String::new(), Vec::new(), HashSet::new()),
        };

        Self {
            arena,
            offsets,
            hashes,
            path,
        }
    }

    /// Create from pre-existing entries (for testing/benchmarks).
    pub fn from_entries(entries: Vec<String>) -> Self {
        let total: usize = entries.iter().map(|e| e.len()).sum();
        let mut arena = String::with_capacity(total);
        let mut offsets = Vec::with_capacity(entries.len());
        let mut hashes = HashSet::with_capacity(entries.len());
        for e in &entries {
            let start = arena.len() as u32;
            arena.push_str(e);
            offsets.push((start, e.len() as u16));
            hashes.insert(hash_str(e));
        }
        Self {
            arena,
            offsets,
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
            // Likely duplicate — remove prior occurrence
            self.offsets.retain(|&(start, len)| {
                &self.arena[start as usize..start as usize + len as usize] != line
            });
        }
        let start = self.arena.len() as u32;
        self.arena.push_str(line);
        self.offsets.push((start, line.len() as u16));
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
}
