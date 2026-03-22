# Search and Ranking — Design Notes

This document covers the ranking and scoring strategies for history search
and tab completion.

## History Fuzzy Search

### Matching

`history.rs` uses **subsequence matching** with optimal alignment: every
character of the query must appear in the entry in order (case-insensitive).
The matcher uses a three-pass approach to find the tightest match window:

1. **Forward pass**: greedy scan to confirm the query is a subsequence and
   find the first endpoint where the last query character matches.
2. **Last-endpoint scan**: find the last occurrence of the last query char
   in the text. If different from the first endpoint, try both.
3. **Backward pass**: from each endpoint, scan backward matching query chars
   in reverse to find the tightest (shortest span) window.
4. **Final forward pass**: within the winning window, record positions.

This finds contiguous matches that a greedy-only approach would miss. For
example, searching "test" in "the best test" finds the contiguous "test" at
the end (positions 9-12), not the scattered t(0)-e(2)-s(6)-t(7) that a
greedy forward pass would produce. Similarly, "deb" in "cd target/debug"
finds the contiguous d(10)-e(11)-b(12) in "debug".

ASCII fast path avoids char decoding overhead. The Unicode path collects
chars into a Vec for the backward scan (rare path, short entries).

### Scoring

Each `FuzzyMatch` carries a `score: i16` computed by `score_match()` from
the positions array. Scoring signals, in order of impact:

**First-match bonus (+4)**
Extra weight when the first query character matches position 0 of the
entry. Typing `g` strongly prefers entries starting with `g`.

**Contiguity bonus (+16 per consecutive match)**
The most important signal. If matched characters are adjacent in the entry,
the query is likely a substring or near-substring. Six contiguous matches
(like "target" appearing literally) score 80+ from this alone. Scattered
matches across a long string score near 0.

**Word boundary bonus (+8)**
A matched character at the start of a word — after `/`, `-`, `_`, `.`,
whitespace, or at position 0 — is worth more than one mid-word. This makes
path-component matching and command-name matching work naturally.

**Gap penalty (-1 per skipped char, capped at -3 per gap)**
Penalizes distance between consecutive matches, but the cap prevents a
single long gap (common in paths) from destroying an otherwise good match.

**PWD context bonus (+20)**
If the entry contains the current directory's basename (case-insensitive),
it gets a flat bonus. This surfaces commands relevant to the current project
without requiring explicit path tracking. The basename is extracted from
`$PWD` via `getenv` (zero allocation).

### Sort Order

Results are collected most-recent-first up to the limit (200), then sorted
by score descending with recency (entry index) as tiebreaker. A great match
from 20 commands ago beats a terrible match from 2 commands ago, but two
equally-scored matches sort by recency.

### Comparison to fzf

The matching and scoring approach is similar to fzf v2: forward+backward
alignment to find tight windows, contiguity bonuses, word boundary bonuses.
Key differences:

- fzf supports extended syntax (`^prefix`, `suffix$`, `!exclude`, `'exact`)
  with boolean composition. We only support plain fuzzy queries.
- fzf uses a DP-based scoring pass within the window for globally optimal
  alignment. We use a simpler two-endpoint approach (try the first and last
  possible endpoints, pick the tighter window). This covers most real-world
  cases without O(m*n) cost.

## Tab Completion

### Matching

`complete.rs` uses a two-tier strategy:

1. **Prefix match** (high priority): entry name starts with the typed prefix
2. **Substring match** (fallback): entry name contains the prefix (like fish)

If any prefix matches exist, substring-only matches are discarded.

### Sorting: mtime

Path completions are sorted by modification time (most recent first) with
case-insensitive alphabetical as tiebreaker. The `st_mtime` is captured
from the `stat()` call already made per candidate for type detection, so
there is near-zero additional cost.

This naturally surfaces recently-built artifacts (`./target/debug/ish` after
`cargo build`) and recently-edited files above stale ones.

Non-path completions (builtins, commands, hostnames) have `mtime: 0` and
fall back to alphabetical ordering.

### Future: Frecency

Track which completions are actually accepted (command name + argument) and
weight by frequency x recency. This requires persistent storage (a small
file, similar to history) and is a larger change. The mtime approach gives
80% of the benefit for 10% of the complexity.

### Future: History-Informed Completion

When completing a path prefix, check recent history for commands containing
paths that match the prefix. If the user recently ran
`./target/debug/ish --help`, then typing `./ta` should surface that path
even before reading the filesystem. This bridges completion and history into
a unified relevance model.

## Design Constraints

All search and ranking code respects ish's core invariants:

- **Zero allocation on warm paths.** Scoring uses the existing `[u16; 32]`
  positions array and a fixed `i16` score field. No allocations per match.
  `pwd_basename` is a borrowed `&str` from the env block.
- **Single dependency (libc).** No external fuzzy matching libraries.
- **Bounded work.** History search caps at 200 results. Completion caps at
  readdir output. Scoring is O(match_count) for ASCII text per candidate.
- **Predictable.** Users can reason about why a result ranks where it does.
  Contiguity + word boundaries + recency is intuitive.
