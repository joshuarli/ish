# Search and ranking

Design notes for the three interactive search surfaces: Ctrl+R history search,
Tab completion, and Ctrl+F file finding. This document records behavior that
is easy to accidentally change while optimizing or refactoring the hot path.

## History search

The implementation lives in `src/history.rs`:

- `History::fuzzy_search_into()` is the normal Ctrl+R entry point. It reuses a
  caller-owned `Vec<FuzzyMatch>` and keeps at most the requested limit.
- `History::fuzzy_search_subset_into()` narrows an existing candidate set;
  `src/main.rs::handle_history_search_key()` uses it while the query grows or
  shrinks.
- `FuzzyMatch` stores the entry index, up to 32 match positions, match count,
  and the tier score used for ordering.
- `classify_match()` selects the first applicable match tier. The ASCII path is
  `classify_match_ascii()`; the alignment helpers are
  `subsequence_match_ascii_bytes()` and `subsequence_match_unicode()`.
- `compare_fuzzy_match()` sorts stronger tiers first, then newer history
  entries. `score_match()` and the `pwd_basename` parameter remain for API and
  test compatibility, but current-directory weighting is not active.

Matching is intentionally literal-first:

1. Case-insensitive prefix match, score `3`.
2. Case-insensitive substring beginning at a word boundary, score `2`.
3. Case-insensitive substring anywhere, score `1`.
4. Case-insensitive subsequence fallback, score `0`.

Within a tier, newer entries win. Subsequence matches record positions so the
history pager can highlight them. Empty queries return visible history in
recency order. Ctrl+R normally asks for 200 results; the limit is supplied by
the caller rather than hard-coded into the matching primitive.

The UI and rendering call sites are:

- `Mode::HistorySearch` in `src/main.rs` owns the query, candidates, matches,
  and selection.
- `handle_history_search_key()` handles editing, incremental filtering, and
  selection.
- `render_history_mode()` calls
  `render::render_history_pager_cached()` in `src/render.rs`.

When changing ranking, update the focused history tests in `src/history.rs` and
the integration/PTY coverage in `tests/integration.rs` and `tests/pty.rs`.
Keep ranking predictable and recency-friendly; do not add a general fuzzy
matching dependency without an explicit design decision.

## Tab completion

The data model and filesystem search are in `src/complete.rs`:

- `Completions` is an arena: all names are stored in `names`, while
  `CompletionEntry` stores offsets, lengths, flags, and modification time.
- `complete_path_into()` completes a filesystem path into a caller-owned arena.
- `complete_candidates()` provides deterministic, filesystem-free filtering
  for tests and benchmarks.
- `CompletionState` owns grid selection, scrolling, directory prefix, and
  quoting state.
- `compute_grid()` calculates the column-major layout without heap allocation.
- `sort_by_mtime()` orders candidates by newest `st_mtime`, with
  case-insensitive name order as the tiebreaker. Non-path candidates use mtime
  zero.

Matching is two-stage. Prefix matches are preferred; case-insensitive
substring matches are retained only when there are no prefix matches. Hidden
entries are excluded unless the typed prefix begins with `.`. `dirs_only`
filters out non-directories. Duplicate names are removed by
`Completions::dedup_sorted()` after sorting.

Completion orchestration is in `src/main.rs`:

- `start_completion()` decides whether to complete commands, builtins,
  hostnames, remote paths, or filesystem paths.
- `handle_completion_key()` handles navigation, refiltering, acceptance, and
  cancellation.
- `preview_completion()` updates the line while navigating without committing
  the selection.
- `render::render_completions()` draws the grid.

Keep the warm path allocation-conscious. `complete_path_into()` exists so the
shell can reuse `Shell::completion_buffer`; use it instead of constructing a fresh
`Completions` value in per-keystroke code. If completion ordering changes,
update `complete_candidates()` tests in `src/complete.rs` and the completion
tests in `tests/integration.rs`/`tests/pty.rs`.

## File finder

Ctrl+F is a separate filesystem search, not path completion. Its implementation
is in `src/finder.rs`:

- `find()` is the synchronous/testable search primitive.
- `find_async()` returns a `FinderHandle`; `drain_into()` collects results and
  `stop()` cancels the worker.
- `load_gitignores()`, `parse_gitignore()`, and `is_ignored()` implement the
  default gitignore-aware filtering.
- `walk_hidden()` is used for the hidden-results mode.
- `filter_entries_pub()` exposes deterministic filtering behavior to tests.

Results are represented as `(depth, path)` pairs internally and are presented
shallowest-first. Normal mode hides dotfiles and applies gitignore rules;
hidden mode disables those filters. Keep traversal bounded and cancellable,
and test both modes when changing ignore or traversal behavior.

## Design constraints

- Prefer the standard library or existing dependencies. Do not add a fuzzy
  matching or completion crate for these features.
- Keep matching deterministic, case-insensitive where documented, and stable
  under recency/mtime ties.
- Preserve bounded interactive work: history callers cap displayed results,
  completion scans one directory at a time, and finder searches can be
  cancelled.
- Keep allocation-sensitive code explicit. Reuse the buffers and arenas passed
  into the `*_into()` APIs.
- Preserve comments that explain ranking choices, allocation behavior, or
  platform-specific filesystem decisions.

Future ideas such as frecency or history-informed completion require persistent
state and a clearer product decision; they are not part of the current ranking
contract.
