pub mod cli;
mod terminal;
mod termination;
mod picker;
pub use picker::{CandidateId, Outcome, Picker, PickerConfig};

mod score;
pub use score::{find_either, has_match, match_score, match_positions};

fn rank_into(needle: &[u8], candidates: &[Vec<u8>], results: &mut Vec<(usize, f64)>) {
    results.clear();
    let mut scorer = score::ScoreWorkspace::new();
    results.extend(candidates.iter().enumerate().filter(|&(_index, candidate)| has_match(needle, candidate)).map(|(index, candidate)| (index, scorer.score(needle, candidate))));
    // Index makes ties deterministic without allocating a stable-sort buffer.
    results.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

/// Eligible candidate indices in descending score order, retaining input order
/// for ties. Candidate bytes are never copied while searching or sorting.
pub fn rank(needle: &[u8], candidates: &[Vec<u8>]) -> Vec<(usize, f64)> {
    let mut results = Vec::new();
    rank_into(needle, candidates, &mut results);
    results
}

fn by_rank(a: &(usize, f64), b: &(usize, f64)) -> std::cmp::Ordering {
    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

/// Results ranked on first use. The first `ranked` entries of `results` are in
/// final order and every later entry ranks after them. A screen shows a few
/// rows, so ranking only those costs a linear selection instead of sorting
/// every match. Accessors take `&mut self` because reading a row can extend
/// the ranked prefix.
///
/// A search resets selection to the first result. Before the first search, no
/// result is available. Navigation wraps, and empty results retain index zero.
pub struct Choices {
    candidates: Candidates,
    all: bool,
    query: Vec<u8>,
    workers: usize,
    results: Vec<(usize, f64)>,
    ranked: usize,
    selection: usize,
}

// Smallest prefix ranked at once, enough for a full screen plus paging.
const MIN_RANKED: usize = 64;

// Candidate bytes times query bytes at which spawning workers pays for itself.
// Measured on 100k short records: 2,000,000 left searches serial that finish
// about a millisecond sooner when split, while 200,000 was no better than 600,000.
const PARALLEL_WORK: usize = 600_000;

impl Choices {
    pub fn new(candidates: Vec<Vec<u8>>) -> Self {
        Self::from_candidates(Candidates::from_records(candidates))
    }

    pub fn from_candidates(candidates: Candidates) -> Self {
        Self { candidates, results: Vec::new(), ranked: 0, selection: 0, all: false, query: Vec::new(), workers: 1 }
    }

    /// Set a worker ceiling. Zero uses available CPUs; at most four workers
    /// participate, and small searches stay on the caller to avoid startup cost.
    pub fn set_workers(&mut self, workers: usize) {
        self.workers = if workers == 0 {
            std::thread::available_parallelism().map(usize::from).unwrap_or(1)
        } else { workers }.clamp(1, 4);
    }

    pub fn search(&mut self, needle: &[u8]) {
        self.selection = 0;
        self.ranked = 0;
        let refine = !self.query.is_empty() && needle.starts_with(&self.query);
        self.query.clear();
        self.query.extend_from_slice(needle);
        self.all = needle.is_empty();
        // The empty query is already in input order; no scoring, sorting, or
        // result allocation is needed for the initial interactive screen.
        if self.all { self.results.clear(); return; }
        if refine {
            let mut scorer = score::ScoreWorkspace::new();
            // Extending a subsequence query can only remove eligible matches.
            // Editing or deleting its earlier bytes requires a full search.
            self.results.retain_mut(|(index, score)| {
                let candidate = self.candidates.get(*index).unwrap();
                if has_match(needle, candidate) {
                    *score = scorer.score(needle, candidate);
                    true
                } else { false }
            });
        } else {
            self.results.clear();
            // Reserve once. Growing dense results geometrically retains old
            // allocator pages and inflates peak RSS even after realloc frees them.
            self.results.reserve_exact(self.candidates.len());
            let work = self.candidates.bytes.len().saturating_mul(needle.len());
            if self.workers > 1 && self.candidates.len() >= 4096 && work >= PARALLEL_WORK {
                let chunk = self.candidates.len().div_ceil(self.workers);
                // Workers read disjoint candidate ranges and own only their
                // share of the result capacity, unlike fzy's N-sized per-worker
                // buffers. The caller participates and performs one final sort.
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(self.workers - 1);
                    let candidates = &self.candidates;
                    for start in (chunk..candidates.len()).step_by(chunk) {
                        let end = (start + chunk).min(candidates.len());
                        handles.push(scope.spawn(move || {
                            let mut results = Vec::with_capacity(end - start);
                            score_range(needle, candidates, start, end, &mut results);
                            results
                        }));
                    }
                    score_range(needle, candidates, 0, chunk, &mut self.results);
                    for handle in handles { self.results.extend(handle.join().unwrap()); }
                });
            } else {
                score_range(needle, &self.candidates, 0, self.candidates.len(), &mut self.results);
            }
        }
    }

    /// Rank every result now. Cheaper than reading rows one at a time when
    /// all of them are needed, as when printing the whole list.
    pub fn rank_all(&mut self) {
        self.rank_through(usize::MAX);
    }

    // Grows the prefix at least fourfold so paging stays near-linear overall,
    // and sorts outright once the prefix would cover a quarter of the results.
    fn rank_through(&mut self, count: usize) {
        let len = self.results.len();
        let count = count.min(len);
        if count <= self.ranked { return; }
        let target = count.max(self.ranked.saturating_mul(4)).max(MIN_RANKED).min(len);
        let tail = &mut self.results[self.ranked..];
        if target >= len.div_ceil(4) {
            tail.sort_unstable_by(by_rank);
            self.ranked = len;
        } else {
            let prefix = target - self.ranked;
            tail.select_nth_unstable_by(prefix - 1, by_rank);
            tail[..prefix].sort_unstable_by(by_rank);
            self.ranked = target;
        }
    }

    pub fn next(&mut self) {
        if self.available() > 0 {
            self.selection = (self.selection + 1) % self.available();
        }
    }

    pub fn prev(&mut self) {
        if self.available() > 0 {
            self.selection = (self.selection + self.available() - 1) % self.available();
        }
    }

    /// Stable input identity for a ranked candidate, independent of query changes.
    pub fn source_index(&mut self, index: usize) -> Option<usize> {
        if self.all { return (index < self.candidates.len()).then_some(index); }
        if index >= self.results.len() { return None; }
        self.rank_through(index + 1);
        Some(self.results[index].0)
    }

    /// Access a candidate in original input order (also distinguishes duplicates).
    pub fn source(&self, index: usize) -> Option<&[u8]> { self.candidates.get(index) }

    pub fn get(&mut self, index: usize) -> Option<&[u8]> {
        let source = self.source_index(index)?;
        self.source(source)
    }

    pub fn getscore(&mut self, index: usize) -> Option<f64> {
        if self.all { return (index < self.candidates.len()).then_some(f64::NEG_INFINITY); }
        if index >= self.results.len() { return None; }
        self.rank_through(index + 1);
        Some(self.results[index].1)
    }

    pub fn selected(&mut self) -> Option<&[u8]> { self.get(self.selection) }
    pub fn len(&self) -> usize { self.candidates.len() }
    pub fn is_empty(&self) -> bool { self.candidates.is_empty() }
    pub fn available(&self) -> usize { if self.all { self.candidates.len() } else { self.results.len() } }
    pub fn selection(&self) -> usize { self.selection }
}

fn score_range(needle: &[u8], candidates: &Candidates, start: usize, end: usize, results: &mut Vec<(usize, f64)>) {
    let mut scorer = score::ScoreWorkspace::new();
    for index in start..end {
        let candidate = candidates.get(index).unwrap();
        if has_match(needle, candidate) {
            results.push((index, scorer.score(needle, candidate)));
        }
    }
}

/// Candidate bytes share one allocation. Adjacent offsets delimit records, so
/// indexing costs four bytes per candidate for inputs up to 4 GiB minus one
/// byte, or one machine word for larger inputs, with no allocation per line.
/// Input records retain their separators; only the offset index is allocated.
/// Runs of empty records are skipped when indexing and trimmed when borrowing.
pub struct Candidates {
    bytes: Vec<u8>,
    offsets: CandidateOffsets,
    delimiter: Option<u8>,
}

// Most inputs fit in 4 GiB. Store their positions in four bytes, retaining
// machine-sized offsets for larger inputs rather than imposing an input limit.
enum CandidateOffsets {
    Compact(Vec<u32>),
    Wide(Vec<usize>),
}

impl CandidateOffsets {
    fn push(&mut self, offset: usize) {
        match self {
            Self::Compact(offsets) => offsets.push(u32::try_from(offset).expect("compact input offset")),
            Self::Wide(offsets) => offsets.push(offset),
        }
    }

    fn len(&self) -> usize {
        match self { Self::Compact(offsets) => offsets.len(), Self::Wide(offsets) => offsets.len() }
    }

    fn get(&self, index: usize) -> Option<usize> {
        match self {
            Self::Compact(offsets) => offsets.get(index).map(|&offset| offset as usize),
            Self::Wide(offsets) => offsets.get(index).copied(),
        }
    }

    fn shrink_to_fit(&mut self) {
        match self { Self::Compact(offsets) => offsets.shrink_to_fit(), Self::Wide(offsets) => offsets.shrink_to_fit() }
    }
}

impl Candidates {
    pub fn from_input(mut bytes: Vec<u8>, delimiter: u8) -> Self {
        let capacity = bytes.len() / 48 + 1;
        let mut offsets = if u32::try_from(bytes.len()).is_ok() {
            CandidateOffsets::Compact(Vec::with_capacity(capacity))
        } else {
            CandidateOffsets::Wide(Vec::with_capacity(capacity))
        };
        let mut start = 0;
        while start < bytes.len() {
            let end = start + score::find_either(&bytes[start..], delimiter, 0)
                .unwrap_or(bytes.len() - start);
            if start < end { offsets.push(start); }
            if end == bytes.len() { break; }
            if delimiter != 0 && bytes[end] == 0 {
                bytes.truncate(end);
                break;
            }
            start = end + 1;
        }
        offsets.push(bytes.len());
        offsets.shrink_to_fit();
        bytes.shrink_to_fit();
        Self { bytes, offsets, delimiter: Some(delimiter) }
    }

    pub fn from_records(records: Vec<Vec<u8>>) -> Self {
        let mut bytes = Vec::with_capacity(records.iter().map(Vec::len).sum());
        let mut offsets = Vec::with_capacity(records.len() + 1);
        for record in records {
            offsets.push(bytes.len());
            bytes.extend_from_slice(&record);
        }
        offsets.push(bytes.len());
        Self { bytes, offsets: CandidateOffsets::Wide(offsets), delimiter: None }
    }

    pub fn len(&self) -> usize { self.offsets.len() - 1 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let mut end = self.offsets.get(index.checked_add(1)?)?;
        let start = self.offsets.get(index)?;
        if let Some(delimiter) = self.delimiter {
            while end > start && self.bytes[end - 1] == delimiter { end -= 1; }
        }
        Some(&self.bytes[start..end])
    }
}
