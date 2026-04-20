# Search and Ranking — Design Notes

This document covers the ranking and scoring strategies for history search
and tab completion.

## History Search

### Matching

`history.rs` now uses a **tiered literal-first search** for Ctrl+R:

1. **Prefix match**: entry starts with the query
2. **Boundary substring**: entry contains the query after `/`, `-`, `_`, `.`, or whitespace
3. **Substring match**: entry contains the query anywhere
4. **Subsequence fallback**: only if there is no literal substring match in that entry

The first three tiers are easy to reason about and align better with shell
history use: if you type a literal command fragment, the most recent literal
match should surface first. Subsequence matching remains as a fallback for
short abbreviations like `gc`.

When the search falls back to subsequence matching, it still uses the
forward/backward alignment pass to highlight the tightest window.

### Scoring

Each `FuzzyMatch` carries a simple tier score:

- `3` prefix
- `2` boundary substring
- `1` substring
- `0` subsequence fallback

Within a tier, newer entries win. There is no current-directory bonus and no
weighted reranking pass.

### Sort Order

Results are ranked by tier first, then by recency. The full result set is
ranked before truncation, so an older but stronger literal match is not
dropped just because 200 newer weak matches appeared first.

### Comparison to fzf

This is intentionally less ambitious than fzf-style scoring. The design goal
is predictability, not maximum fuzzy cleverness.

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

- **Single dependency (libc).** No external fuzzy matching libraries.
- **Bounded work.** History search ranks the full match set, then caps the
  displayed results at 200. Completion caps at readdir output.
- **Predictable.** Users can reason about why a result ranks where it does:
  literal matches first, then recency.
