// Scoring behavior derived from fzy, copyright (c) 2014
// John Hawthorn. SPDX-License-Identifier: MIT. See LICENSE.

const MATCH_MAX_LEN: usize = 1024;
const GAP_LEADING: f64 = -0.005;
const GAP_TRAILING: f64 = -0.005;
const GAP_INNER: f64 = -0.01;
const CONSECUTIVE: f64 = 1.0;
const BONUS_NONE: u8 = 0;
const BONUS_SLASH: u8 = 1;
const BONUS_WORD: u8 = 2;
const BONUS_DOT: u8 = 3;
const BONUS_CAPITAL: u8 = 4;
const BYTE_WORD_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_WORD_HIGHS: u64 = 0x8080_8080_8080_8080;

#[inline]
fn zero_byte_mask(word: u64) -> u64 {
    word.wrapping_sub(BYTE_WORD_ONES) & !word & BYTE_WORD_HIGHS
}

#[inline]
pub fn find_either(haystack: &[u8], first: u8, second: u8) -> Option<usize> {
    let first_word = u64::from(first) * BYTE_WORD_ONES;
    let second_word = u64::from(second) * BYTE_WORD_ONES;
    let (words, remainder) = haystack.as_chunks::<8>();
    for (index, chunk) in words.iter().enumerate() {
        let word = u64::from_le_bytes(*chunk);
        let mut matches = zero_byte_mask(word ^ first_word);
        if second != first {
            matches |= zero_byte_mask(word ^ second_word);
        }
        if matches != 0 {
            return Some(index * 8 + matches.trailing_zeros() as usize / 8);
        }
    }
    let offset = haystack.len() - remainder.len();
    remainder.iter().position(|&ch| ch == first || ch == second).map(|index| offset + index)
}

/// Subsequence eligibility follows fzy's asymmetric ASCII rule: a lowercase
/// query byte accepts either case; an uppercase byte accepts only itself.
/// Inputs are byte strings without C terminators; non-ASCII bytes match exactly.
pub fn has_match(needle: &[u8], mut haystack: &[u8]) -> bool {
    for &byte in needle {
        let uppercase = byte.to_ascii_uppercase();
        if let Some((&first, rest)) = haystack.split_first()
            && (first == byte || first == uppercase)
        {
            haystack = rest;
            continue;
        }
        let Some(position) = find_either(haystack, byte, uppercase) else {
            return false;
        };
        haystack = &haystack[position + 1..];
    }
    true
}

#[inline]
fn bonus(previous: u8, current: u8) -> u8 {
    if !current.is_ascii_alphanumeric() {
        return BONUS_NONE;
    }
    match previous {
        b'/' => BONUS_SLASH,
        b'-' | b'_' | b' ' => BONUS_WORD,
        b'.' => BONUS_DOT,
        b'a'..=b'z' if current.is_ascii_uppercase() => BONUS_CAPITAL,
        _ => BONUS_NONE,
    }
}

#[inline]
fn bonus_score(bonus: u8) -> f64 {
    match bonus {
        BONUS_SLASH => 0.9,
        BONUS_WORD => 0.8,
        BONUS_DOT => 0.6,
        BONUS_CAPITAL => 0.7,
        _ => 0.0,
    }
}

#[derive(Clone, Copy)]
struct Score {
    ending: f64,
    best: f64,
}

#[inline]
fn lowercase(byte: u8) -> u8 { byte.to_ascii_lowercase() }

// The reference macOS Clang build fuses this expression when FMA is available;
// the Linux GCC build with upstream's -std=c99 flags rounds it twice. Keep
// those defaults: one ULP can change the order of otherwise equal matches.
fn initial_score(column: usize, bonus: f64) -> f64 {
    #[cfg(all(target_os = "macos", any(target_arch = "aarch64", target_feature = "fma")))]
    { (column as f64).mul_add(GAP_LEADING, bonus) }
    #[cfg(not(all(target_os = "macos", any(target_arch = "aarch64", target_feature = "fma"))))]
    { column as f64 * GAP_LEADING + bonus }
}

/// Scratch storage shared by all candidates in one search. Candidate bytes are
/// folded and their bonus is encoded once, while the fixed fzy-sized rows are
/// initialized only once per search.
pub(crate) struct ScoreWorkspace {
    ending: [f64; MATCH_MAX_LEN],
    best: [f64; MATCH_MAX_LEN],
    lowercase_haystack: [u8; MATCH_MAX_LEN],
    bonus_codes: [u8; MATCH_MAX_LEN],
}

impl ScoreWorkspace {
    pub(crate) fn new() -> Self {
        Self {
            ending: [f64::NEG_INFINITY; MATCH_MAX_LEN],
            best: [f64::NEG_INFINITY; MATCH_MAX_LEN],
            lowercase_haystack: [0; MATCH_MAX_LEN],
            bonus_codes: [BONUS_NONE; MATCH_MAX_LEN],
        }
    }

    /// Scores with the same edge cases as [`match_score`], reusing its buffers.
    pub(crate) fn score(&mut self, needle: &[u8], haystack: &[u8]) -> f64 {
        if needle.is_empty() || haystack.len() > MATCH_MAX_LEN || needle.len() > haystack.len() {
            return f64::NEG_INFINITY;
        }
        if needle.len() == haystack.len() {
            return if needle.eq_ignore_ascii_case(haystack) { f64::INFINITY } else { f64::NEG_INFINITY };
        }
        score_rows(self, needle, haystack, |_, _| {})
    }

    fn prepare(&mut self, haystack: &[u8]) {
        let mut previous = b'/';
        for (j, &ch) in haystack.iter().enumerate() {
            self.lowercase_haystack[j] = lowercase(ch);
            self.bonus_codes[j] = bonus(previous, ch);
            previous = ch;
        }
    }
}

// Each row represents one more query byte. `ending` requires a match at this
// candidate position; `best` may end in a gap. Keeping both prevents a word-start
// bonus from stacking with the consecutive-letter bonus. The first row overwrites
// every cell, so reused storage never needs a per-candidate initialization.
#[inline]
fn score_rows(
    workspace: &mut ScoreWorkspace,
    needle: &[u8],
    haystack: &[u8],
    mut record: impl FnMut(Score, bool),
) -> f64 {
    workspace.prepare(haystack);
    let width = haystack.len();
    let first_gap = if needle.len() == 1 { GAP_TRAILING } else { GAP_INNER };
    let first_byte = lowercase(needle[0]);
    let mut best = f64::NEG_INFINITY;

    for j in 0..width {
        let ending;
        if first_byte == workspace.lowercase_haystack[j] {
            ending = initial_score(j, bonus_score(workspace.bonus_codes[j]));
            best = ending.max(best + first_gap);
        } else {
            ending = f64::NEG_INFINITY;
            // Scores are finite or -infinity, so this is exactly
            // `max(-infinity, best + first_gap)`. It follows fzy's mismatch
            // path without a floating-point maximum.
            best += first_gap;
        }
        workspace.ending[j] = ending;
        workspace.best[j] = best;
        record(Score { ending, best }, false);
    }

    for i in 1..needle.len() {
        let gap = if i + 1 == needle.len() { GAP_TRAILING } else { GAP_INNER };
        let byte = lowercase(needle[i]);
        let mut diagonal_ending = f64::NEG_INFINITY;
        let mut diagonal_best = f64::NEG_INFINITY;
        let mut best = f64::NEG_INFINITY;
        for j in 0..width {
            let previous_ending = workspace.ending[j];
            let previous_best = workspace.best[j];
            let ending;
            if byte == workspace.lowercase_haystack[j] && j > 0 {
                ending = (diagonal_best + bonus_score(workspace.bonus_codes[j])).max(diagonal_ending + CONSECUTIVE);
                best = ending.max(best + gap);
            } else {
                ending = f64::NEG_INFINITY;
                // As above, the impossible ending state cannot improve best.
                best += gap;
            }
            workspace.ending[j] = ending;
            workspace.best[j] = best;
            record(Score { ending, best }, j > 0 && best == diagonal_ending + CONSECUTIVE);
            diagonal_ending = previous_ending;
            diagonal_best = previous_best;
        }
    }
    workspace.best[width - 1]
}

/// Optimal fzy score. Empty queries, nonmatches, and candidates over 1024 bytes
/// score -infinity; exact matches score +infinity. Unlike eligibility, scoring
/// folds both ASCII cases, as upstream does. Call `has_match` before ranking.
pub fn match_score(needle: &[u8], haystack: &[u8]) -> f64 {
    if needle.is_empty() || haystack.len() > MATCH_MAX_LEN || needle.len() > haystack.len() {
        return f64::NEG_INFINITY;
    }
    if needle.len() == haystack.len() {
        return if needle.eq_ignore_ascii_case(haystack) { f64::INFINITY } else { f64::NEG_INFINITY };
    }
    let mut workspace = ScoreWorkspace::new();
    score_rows(&mut workspace, needle, haystack, |_, _| {})
}

/// Byte positions for an optimal alignment, choosing the latest path on ties.
/// None means no alignment or an oversized candidate; an empty query has an
/// empty alignment. Only displayed candidates build a compact backtrace map.
pub fn match_positions(needle: &[u8], haystack: &[u8]) -> Option<Vec<usize>> {
    if needle.is_empty() {
        return Some(Vec::new());
    }
    if haystack.len() > MATCH_MAX_LEN || needle.len() > haystack.len() {
        return None;
    }
    if needle.len() == haystack.len() {
        return needle.eq_ignore_ascii_case(haystack).then(|| (0..needle.len()).collect());
    }
    let width = haystack.len();
    const ENDING: u8 = 1;
    const BEST: u8 = 2;
    const CONSECUTIVE_PATH: u8 = 4;

    let mut workspace = ScoreWorkspace::new();
    let mut trace = Vec::with_capacity(needle.len() * width);
    let score = score_rows(&mut workspace, needle, haystack, |cell, consecutive| {
        let flags = ((cell.ending != f64::NEG_INFINITY) as u8 * ENDING) | ((cell.ending == cell.best) as u8 * BEST) | (consecutive as u8 * CONSECUTIVE_PATH);
        trace.push(flags);
    });
    if score == f64::NEG_INFINITY {
        return None;
    }
    let mut positions = vec![0; needle.len()];
    let mut column = width;
    let mut required = false;
    for i in (0..needle.len()).rev() {
        loop {
            column = column.checked_sub(1)?;
            let flags = trace[i * width + column];
            if flags & ENDING != 0 && (required || flags & BEST != 0) {
                required = i > 0 && column > 0 && flags & CONSECUTIVE_PATH != 0;
                positions[i] = column;
                break;
            }
        }
    }
    Some(positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_has_match(needle: &[u8], mut haystack: &[u8]) -> bool {
        for &byte in needle {
            let uppercase = byte.to_ascii_uppercase();
            let Some(position) = haystack.iter().position(|&ch| ch == byte || ch == uppercase) else {
                return false;
            };
            haystack = &haystack[position + 1..];
        }
        true
    }

    #[test]
    fn has_match_agrees_with_scalar_reference_across_word_boundaries() {
        let mut state = 0x163a_9a31_9c58_4de7_u64;
        for width in 0..=40 {
            for _ in 0..100 {
                let mut haystack = vec![0; width];
                let mut needle = Vec::with_capacity(10);
                for byte in &mut haystack {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                for _ in 0..state as usize % 11 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    needle.push(state as u8);
                }
                assert_eq!(has_match(&needle, &haystack), scalar_has_match(&needle, &haystack));
            }
        }
    }

    #[test]
    fn find_either_returns_the_earliest_byte_across_word_boundaries() {
        let bytes = b"1234567\0abcdef\n";
        assert_eq!(find_either(bytes, b'\n', 0), Some(7));
        assert_eq!(find_either(bytes, b'x', b'\n'), Some(14));
        assert_eq!(find_either(bytes, b'x', b'x'), None);
    }

    #[test]
    fn positions_preserve_fzys_latest_backtrace_across_tied_rows() {
        // Upstream's greedy latest backtrace follows this tied DP state through
        // positions 3, 4, and 25. The compact trace must preserve that choice.
        assert_eq!(
            match_positions(b"BBBZA", b".c-BB AA 0aa._ZaCZ/CC c BBzACaB9ZZaa-_A/AAZ"),
            Some(vec![3, 4, 25, 26, 27]),
        );
    }

    #[test]
    fn workspace_reuses_rows_across_candidate_widths() {
        let mut workspace = ScoreWorkspace::new();
        for (needle, haystack) in [
            (b"a".as_slice(), b"*a".as_slice()),
            (b"abc".as_slice(), b"a--b--c".as_slice()),
            (b"a".as_slice(), b"a".as_slice()),
            (b"a".as_slice(), b"....................................a".as_slice()),
            (b"a".as_slice(), b"/a".as_slice()),
            (b"z".as_slice(), b"abcdef".as_slice()),
        ] {
            assert_eq!(workspace.score(needle, haystack), match_score(needle, haystack));
        }
    }
}
