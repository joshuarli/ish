use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

pub struct History {
    entries: Vec<String>,
    path: PathBuf,
}

impl History {
    pub fn load() -> Self {
        let path = history_path();
        let entries = if path.exists() {
            match fs::File::open(&path) {
                Ok(f) => {
                    let reader = io::BufReader::new(f);
                    let mut seen = std::collections::HashSet::new();
                    let mut entries: Vec<String> = Vec::new();
                    // Read all lines, dedup keeping most recent
                    let all_lines: Vec<String> = reader
                        .lines()
                        .map_while(Result::ok)
                        .filter(|l| !l.is_empty())
                        .collect();
                    // Iterate in reverse to keep most recent, then reverse back
                    for line in all_lines.into_iter().rev() {
                        if seen.insert(line.clone()) {
                            entries.push(line);
                        }
                    }
                    entries.reverse();
                    entries
                }
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        Self { entries, path }
    }

    /// Add entry. Deduplicates (removes prior occurrence).
    /// Create from pre-existing entries (for testing).
    pub fn from_entries(entries: Vec<String>) -> Self {
        Self {
            entries,
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

        // Remove prior duplicate
        self.entries.retain(|e| e != line);
        self.entries.push(line.to_string());

        // Append to file
        self.append_to_file(line);
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Prefix search: find entries that start with `prefix`, starting from
    /// the end and skipping `skip` matches. Returns the entry text.
    pub fn prefix_search(&self, prefix: &str, skip: usize) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.starts_with(prefix))
            .nth(skip)
            .map(|s| s.as_str())
    }

    /// Fuzzy (subsequence) search: every char of `query` appears in the entry
    /// in order, case-insensitive. Returns matching entries most-recent-first,
    /// with entry index and indices of matching chars.
    pub fn fuzzy_search(&self, query: &str) -> Vec<FuzzyMatch> {
        if query.is_empty() {
            return self
                .entries
                .iter()
                .enumerate()
                .rev()
                .map(|(idx, _)| FuzzyMatch {
                    entry_idx: idx,
                    match_positions: Vec::new(),
                })
                .collect();
        }

        let query_lower: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
        let mut results = Vec::new();

        for (idx, entry) in self.entries.iter().enumerate().rev() {
            if let Some(positions) = subsequence_match(&query_lower, entry) {
                results.push(FuzzyMatch {
                    entry_idx: idx,
                    match_positions: positions,
                });
            }
        }

        results
    }

    /// Get entry text by index.
    pub fn get(&self, idx: usize) -> &str {
        &self.entries[idx]
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

pub struct FuzzyMatch {
    pub entry_idx: usize,
    pub match_positions: Vec<usize>,
}

/// Check if `query` chars appear in `text` in order (case-insensitive).
/// Returns character indices in `text` that matched.
pub fn subsequence_match(query: &[char], text: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }

    // ASCII fast path: if both query and text are ASCII, operate on bytes directly.
    // This avoids char decoding and to_lowercase() overhead for the common case.
    if text.is_ascii() && query.iter().all(|c| c.is_ascii()) {
        let bytes = text.as_bytes();
        let mut positions = Vec::with_capacity(query.len());
        let mut qi = 0;
        let qb = query[qi] as u8; // already lowercase from caller
        let mut target = qb;

        for (ti, &b) in bytes.iter().enumerate() {
            if b.to_ascii_lowercase() == target {
                positions.push(ti);
                qi += 1;
                if qi == query.len() {
                    return Some(positions);
                }
                target = query[qi] as u8;
            }
        }
        return None;
    }

    let mut positions = Vec::with_capacity(query.len());
    let mut qi = 0;

    for (ti, tc) in text.chars().enumerate() {
        if tc.to_lowercase().next() == Some(query[qi]) {
            positions.push(ti);
            qi += 1;
            if qi == query.len() {
                return Some(positions);
            }
        }
    }

    None
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
        let positions = subsequence_match(&q, "git checkout").unwrap();
        assert_eq!(positions, vec![0, 4, 9]);
    }

    #[test]
    fn subsequence_no_match() {
        let q: Vec<char> = "xyz".chars().collect();
        assert!(subsequence_match(&q, "hello").is_none());
    }
}
