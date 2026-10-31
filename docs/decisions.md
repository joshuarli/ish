# Decisions

Accepted decisions about dependencies and data structures. Each record states
the choice, the evidence, and what would reopen it.

## Keep `fxhash` and `std` collections; reject `bbtree`

Status: accepted (2026-10-31).

### Decision

`ish` and `epsh` keep `fxhash::FxHashMap` and `fxhash::FxHashSet` for unordered
lookup, and `std::collections::BTreeMap` and `BTreeSet` for ordered keys. Do not
adopt [`bbtree`](https://github.com/laputa-systems/bbtree).

### Why

`bbtree`'s `unsync::BBTreeMap` and `BBTreeSet` are drop-in replacements for
`std`'s `BTreeMap` and `BTreeSet`, and they win that comparison. They are not
replacements for hash maps. Every unordered lookup measured slower, and one of
them sits in the Ctrl+R ranking loop that runs once per history candidate.

`ish` also never bulk-builds a large ordered collection. The ordered maps it
keeps, `latest_occurrences` and `session` in [`src/history.rs`](../src/history.rs),
turn over one entry at a time. That is the load where the advantage disappears.

### Measurements

Recorded 2026-10-31 on macOS aarch64, release profile. A swap of every
`BTreeMap`, `BTreeSet`, `FxHashMap`, and `FxHashSet` in `ish` and `epsh` was
built, tested, and measured against unmodified `master`.

Collection operations, 20,000 entries, median of 15 interleaved samples:

| Operation | `fxhash` or `std` | `bbtree` | Change |
| --- | --- | --- | --- |
| `u64` set `contains` | 0.88 ns | 14.7 ns | 16.7× slower |
| `u64` map `get` | 0.99 ns | 11.9 ns | 12.0× slower |
| `PathBuf` map `get`, hit | 55 ns | 397 ns | 7.2× slower |
| `PathBuf` map `get`, miss | 33 ns | 452 ns | 13.7× slower |
| `String` map `get`, hit | 6.1 ns | 31.5 ns | 5.2× slower |
| `String` map insert, 20k keys | 0.70 ms | 1.34 ms | 1.9× slower |
| `BTreeSet<u64>` insert, 20k keys | 0.61 ms | 0.073 ms | 8.4× faster |

Whole-shell paths. The host was running another workload, so each ratio
compares adjacent runs rather than separate batches:

| Path | Change |
| --- | --- |
| `History::fuzzy_search_subset_into_in_dir`, 45k entries (Ctrl+R in a directory) | 1.50×–1.64× slower, median 1.54× |
| `History::from_entries_in_dirs`, 45k entries | 1.34×–1.44× slower, median 1.39× |

Returning `directories` in [`src/history.rs`](../src/history.rs) to `FxHashMap`
alone restores Ctrl+R to 0.95×–1.01× of baseline. That map is read once per
candidate, so a tree descent per candidate dominates the search.

Memory, from the allocator counters in `cargo bench --bench bench`:

| Benchmark | Before | After |
| --- | --- | --- |
| `startup_cold` | 937.6 KB, 6995 allocs | 856.3 KB, 6515 allocs |
| `startup_warm` | 405.1 KB, 2982 allocs | 279.9 KB, 2451 allocs |
| `command_enter_to_prompt` | 3.61 KB | 7.23 KB |
| `cd_prompt_with_denv` | 43.23 KB | 48.81 KB |

`bbtree` stores entries in 1 KiB leaf pages. That packing is where the
`startup_*` drop comes from, and it is also why a map that takes one entry pays
a whole page for it.

### Cost

- Release binary: 2.67 MB to 3.11 MB, +16.8%. All of the growth is `bbtree`
  monomorphized across seven key types; no `vstd` symbols survive linking.
- Dependencies: `fxhash` and `byteorder` leave, twelve crates arrive. Fresh
  dependency build time goes from 0.7–1.0 s to 7.6–10.9 s.
- `bbtree` depends on `vstd` unconditionally, and neither it nor the six Verus
  crates behind it resolve from the `stuff` registry in
  [`.cargo/config.toml`](../.cargo/config.toml).
- `epsh::eval::Shell::functions` and `traps` return `&FxHashMap`. Swapping them
  changes a public API that [`crates/epsh/tests/api_stability.rs`](../crates/epsh/tests/api_stability.rs)
  pins.

### Scope

The `hashbrown::HashSet` uses in [`crates/fdu-scan/src/indexed.rs`](../crates/fdu-scan/src/indexed.rs)
and [`crates/fdu-delete/src/lib.rs`](../crates/fdu-delete/src/lib.rs) are the same
unordered-membership shape that measured 7× to 17× slower here, so they stay as
they are. Converting them would also require an `Ord` impl on
`fdu_core::FileIdentity`.

### What would reopen this

- `bbtree` drops the Verus dependency chain, or the `stuff` registry carries it.
- A hot path gains a large ordered collection that `ish` builds in bulk or
  iterates in key order.
- `bbtree` gains a hash-indexed lookup mode that closes the unordered gap.
