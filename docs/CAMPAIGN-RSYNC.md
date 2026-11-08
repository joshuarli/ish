# Campaign: rsync builtin

## Status

Draft, preflighted in place (2026-11-07). Not started. `ready after the owner answers D1-D5`.

## Charter

Add an ish-native `rsync` builtin for local filesystem synchronization with sane defaults: no trailing-directory `/` operand magic, a small explicit option surface, and behavior that leverages `ish-fs` safety/transfer primitives where they fit. The builtin must be equal to or faster than the system `rsync` available on the measured host for the defined local workloads, while providing similar or stronger guarantees for the cases it claims to support. Remote rsync protocol, daemon mode, and rolling network delta transfer are out of scope for this campaign.

Evidence for current state: there is no `rsync` builtin (`src/builtin.rs:161-181`, `rg rsync` only finds SSH completion in `src/editor.rs:1071`). Native file operations are dispatched through `dispatch::plan`/epsh-owned parsing plus ish foreground handlers (`docs/architecture.md:6-22`, `src/builtin.rs:8-22,166,175`, `src/fs_command.rs:26-124`, `src/shell.rs:620+` routes `ExternalCommand::{Copy,Move,...}`; see `rg -n "ExternalCommand::Copy|fs_command::run"`). Reusable local-transfer machinery lives in `crates/ish-fs/src/` (`lib.rs` `Request/Operation/Options/Operands`, `policy.rs` preflight/mapping/collision/identity checks, `engine/transfer.rs` staged+direct copy, reflink/native/buffered file copy, hardlink topology, metadata, publication/retirement receipts, `roots.rs` anchored identity, `platform/*` clone/copy/metadata/sync). `docs/filesystem.md` defines current cp/mv operand, collision, safety, and completion contracts.

## Decisions

### D1 (owner-decided). Command name and shadowing
**Recommendation:** name the builtin `rsync`. Bare `rsync` resolves to the builtin, like `cp`/`mv` (`src/builtin.rs:166,175`, `README.md:3-7,346-351`); `/usr/bin/rsync` and `command /usr/bin/rsync` remain external. Completion/history SSH-command special casing that mentions `rsync` (`src/editor.rs:1071`) stays correct for external remote use but should not imply remote protocol support.
**Why:** matches the goal and user expectation; explicit path preserves escape hatch.
**Reverses if:** owner wants a non-conflicting name (`sync` conflicts with Unix `sync`; `mirror`/`fsync`/other names change muscle memory).

### D2 (owner-decided). Local-only scope and guarantees
**Recommendation:** v1 is local-path synchronization only (source and destination reachable through local filesystem paths/anchored descriptors). No `rsync://`, no `host:`, no `-e ssh`, no daemon protocol, no rolling checksum delta format, no batch files. Guarantees are "similar" where defined and stronger where ish-fs safety applies: no remote-transfer semantics, no partial publication through staged replacement for operations that use ish-fs staging, no following destination symlinks as containers unless an explicit container form is selected, identity-rechecked retirement/publication per existing ish-fs discipline. File-content delta inside a changed local file is **not** required in v1; changed files use whole-file copy/reflink-aware transfer. Quick-check skip is the incremental mechanism.
**Why:** remote protocol/delta is a separate product with network, auth, compatibility, and security surface; the stated pain points are flags and trailing-slash behavior. Reflink/parallel whole-file copy plus skip-unchanged covers common local sync cases.
**Reverses if:** owner requires remote interop or block-delta for large locally changed files in v1.

### D3 (owner-decided). Trailing `/` and operand semantics
**Recommendation:** **never interpret a trailing `/` as "copy contents"**. Path spelling normalization (treating `dir/` as `dir` for existence) is harmless, but operand meaning never changes. Sync uses explicit forms:

- `rsync [OPTIONS] --into DIRECTORY SOURCE...` — synchronize each `SOURCE` into `DIRECTORY` as `DIRECTORY/basename(SOURCE)`. Explicit container form; like cp `--into`.
- `rsync [OPTIONS] SOURCE... DIRECTORY` — if final operand is an existing directory, treat as `--into DIRECTORY` (cp-compatible multi-source convenience). If exactly two operands and final is not an existing directory, treat as exact mapping (below). This preserves cp's existing-directory convenience without slash magic.
- `rsync [OPTIONS] SOURCE DEST` (exact, DEST absent or non-directory) — synchronize `SOURCE` to `DEST`: `DEST` becomes the synchronized object for `SOURCE` (directory contents mirrored, or file copied/updated). This is the explicit "make DEST be this source" form.
- `rsync [OPTIONS] --contents SOURCE DIRECTORY` — synchronize the *contents* of `SOURCE` (must be a directory) into `DIRECTORY` without creating `basename(SOURCE)`. `DIRECTORY` must exist as a directory unless `--make-dirs` is added later (not v1). `--contents` is the explicit replacement for rsync's trailing `/` on source.

Basenames follow cp rules (`policy::into_basename` / existing cp behavior): `/`, `.`/`..` rejected where ambiguous.
**Why:** removes the footgun while keeping an explicit contents operation. Aligns with cp's `--into` and existing-directory mapping where possible.
**Reverses if:** owner wants rsync-compat operand semantics for migration scripts (would require a compat mode, not the default).

### D4 (owner-decided). Default sync policy
**Recommendation:** defaults are safe and unsurprising:
- Recurse into directories automatically.
- **Skip unchanged** regular files by quick check: same entry kind, same logical size, and same mtime (using available timestamp precision; treat incomparable precision conservatively as changed unless proven equal). No `--checksum` in v1.
- Preserve metadata that ish-fs copy preserves for local copies where representable (mode/timestamps/ACL/xattrs/resource forks/platform flags per existing platform code paths), using existing metadata helpers; report limitations as diagnostics, not silent broadening.
- **Do not delete** destination-only entries by default. `--delete` removes destination entries absent from the source mapping, with explicit diagnostics and dry-run visibility.
- Existing destination files that differ are updated. When staged replacement is supported, publish the replacement atomically at the destination name; when staging is unsupported but the operation can proceed safely under an explicit documented fallback (bounded to cases where ish-fs already defines fallback behavior), report that degradation. v1 does not silently leave a half-updated file as "success".
- Symlinks synchronized as symlink objects (not followed as content) by default.
- **Hardlinks (prescribed bound):** v1 does not promise rsync `-H` hardlink topology preservation. The sync planner must not silently create independent copies for a source hardlink group it detected as linked; if hardlink preservation is not implemented for the sync path, it reports a clear diagnostic per affected group/entry and either (a) skips with failure status for that case, or (b) copies content independently only when an explicit future `--split-hardlinks` option is added — default is (a), no data corruption claim. Revisit if owner requires `-H` parity in v1.
- No `--archive` flag bundle; options are explicit and few.
**Why:** "sane defaults" means safe by default, incremental by default, no hidden deletion, no silent semantic loss.
**Reverses if:** owner wants delete-by-default mirror semantics, rsync-archive shorthand, or requires hardlink topology preservation in v1.

### D5 (owner-decided). Progress, dry-run, and output
**Recommendation:** support `--dry-run`, `--verbose`, `--no-progress`, `-j/--jobs` (reuse ish-fs budgeting where applicable), `--delete`, `--contents`, `-n/--dry-run`? No - keep long options primary; accept `-n` only if unambiguous (cp uses `-n` for `--no-clobber`; conflict - **do not** reuse `-n` for dry-run). Accept `--help`/`-h`. No short flag soup. Progress reports files examined, files transferred, files skipped unchanged, bytes transferred, and deletions (when `--delete`) — text adapted from fs_command rendering style but honest about sync-specific counters.
**Why:** avoids flag bloat/conflicts; cp already uses `-n` for no-clobber.
**Reverses if:** owner wants rsync flag compatibility aliases.

### Prescribed (executor settles inside bounds)

- **P1. Implementation shape.** Prefer a new `sync_command.rs` (parallel to `fs_command.rs`) plus a sync engine path. Reuse `crates/ish-fs` `roots`, anchored observation, platform copy/metadata/sync, descriptor/memory budgeting, cancellation/progress plumbing, and quarantine/staging helpers where the sync operation's guarantees match. If sync needs different planning than cp/mv `Operation::{Copy,Move}` (source/dest comparison, deletion planning, update-in-place vs staged replacement), add a distinct `ish-fs` sync request/engine or a `sync` module rather than overloading cp/move policy in ways that risk regressions; keep cp/mv behavior unchanged and covered by existing tests.
- **P2. Quick-check comparison.** Compare anchored observations: regular file unchanged when kind, size, and mtime compare equal under a documented precision rule. Identity/device/inode equality is not required for "same content" across independent syncs but can short-circuit same-run observations. Symlink unchanged when target bytes equal. Directory entries compared by name; metadata updates applied as needed. Recheck identity before mutating/publishing where ish-fs safety requires it.
- **P3. Deletion safety.** `--delete` plans deletions after source enumeration; never deletes before confirming the corresponding source path is absent from the selected mapping. Dry-run lists planned deletions. Deletion uses safe anchored removal; do not follow symlinks out of the destination root. Refuse or bound dangerous roots consistent with existing protected-root philosophy where applicable (document exact rule).
- **P4. Performance proof.** Use the existing fs benchmark discipline (`benches/fs/*`, noisy-host/perf-gate rules from `measure.sh`/`fs_bench.rs`) extended with sync workloads, not one-off laptop timings. Ad-hoc measurements in this doc are baseline evidence only, not acceptance gates. Compare against `/usr/bin/rsync` (openrsync 2.6.9 compatible on macOS host here) and, where available in CI/Linux environments, a documented rsync implementation/version; record implementation+version in results.
- **P5. No formatter churn.** Do not run formatters. Follow repo style.

## Baseline (measured 2026-11-07, macOS 27.2, openrsync 2.6.9 compatible, `/usr/bin/rsync`, local APFS `.work/tmp`, ad-hoc non-gated)

Small tree: 200 files × 4 KiB + 1 × 1 MiB under `src/` (201 files, ~1.78 MiB payload), copied to empty destination, then re-synced unchanged:

| Command | Meaning | Wall time |
|---|---:|---:|
| `rsync -a src/ dst` | fresh copy, contents mode | 0.085 s |
| `rsync -a src/ dst` | second run, mostly unchanged | 0.031 s |
| `rsync -a --delete src/ dst4/` (fresh) | fresh to new dir | real 0.08 s |
| `rsync -a --delete src/ dst4/` (noop) | unchanged | real 0.02 s |

Trailing-slash behavior (openrsync): `rsync -a src dst2/` created `dst2/src/...`; `rsync -a src/ dst3/` copied contents into `dst3/...` with no `dst3/src`. This is the footgun D3 removes.

Existing ish copy capability: `ish-fs` supports parallel transfer, reflink/clone/native/buffered copy, hardlinks, metadata preservation, dry-run/progress, merge/replace/skip collision, staged publication and cross-filesystem move retirement (`crates/ish-fs/src/lib.rs`, `engine/transfer.rs`, `docs/filesystem.md`). It does **not** implement sync planning (destination enumeration, unchanged skip by quick check, extraneous deletion, update of changed entries in place within a destination tree). `cp` without `--merge` treats existing directory as collision (`docs/filesystem.md:85-87`); `--merge` retains destination-only entries and does not skip unchanged by quick check.

Baseline gaps for "rsync-like" local sync:
- no `rsync` command/dispatch/help path (`src/builtin.rs`, `src/fs_command.rs`/new command, `src/shell.rs` external dispatch)
- no sync planner: compare source tree vs destination tree, classify add/update/unchanged/delete
- no quick-check skip
- no `--delete`
- no explicit `--contents` mapping
- no sync-specific progress counters/verbose output
- no sync benchmark workloads/gates

## Sequence

Parity/completeness before optimization. Foundations before parallel feature slices. Every slice has an acceptance command and expected result.

### Phase 0 — Foundation (coordinator or one lane; not parallel)

**Foundation blast-radius proof (required before parallel lanes):** adding `rsync` touches command registration (`src/builtin.rs`), shell/dispatch routing (`src/shell.rs`, `src/dispatch.rs` as needed), and either `src/fs_command.rs` exhaustive operation handling or a new `src/sync_command.rs`. If `crates/ish-fs` gains a sync-specific request/operation, every exhaustive `match` on shared enums becomes a compile-time check — update sites must be found with `cargo check --workspace` and existing cp/mv tests must remain green. Proof before fanning out: `cargo check --workspace && cargo test -p ish-fs && cargo test -p ish --test integration` (or the narrowest existing builtin/fs integration targets that cover cp/mv dispatch) pass with only the new rsync scaffolding added and cp/mv behavior unchanged.

**Deliverable 0.1: Sync design surface and non-goals locked in code-facing doc/tests scaffolding.**
- Add sync request/engine shape per P1 without changing cp/mv behavior.
- Register command/help/dispatch plumbing (`src/builtin.rs` help entry + foreground builtin, `src/shell.rs`/dispatch path, command module). Unknown/invalid options return status 2 with usage; `--help` prints help.
- Proof: `cargo check --workspace`; targeted rsync help/usage tests pass; existing cp/mv tests still pass: `cargo test -p ish-fs` and targeted builtin/fs integration tests covering cp/mv. Threshold: 0 regressions in existing fs/builtin suites touched.

**Deliverable 0.2: Sync benchmark harness extension (minimum gate).**
- Extend `crates/ish-fs/examples/fs_bench.rs` / `benches/fs/measure.sh` vocabulary with sync workloads only after feature semantics exist enough to measure honestly; Phase 0 creates the workload names, fixture plan, and a non-gated smoke measurement script under `.work/tmp` for development. Acceptance gate uses noisy-host/perf-gate rules when `ISH_FS_BENCH_FEATURES_READY` path is used.
- Proof (Phase 0): `cargo test -p ish-fs --example fs_bench --no-run` (or existing build command used by harness) succeeds; smoke script runs in `.work/tmp` and prints elapsed/correctness for fresh+noop+delete cases. Threshold: harness compiles; smoke correctness matches reference tree state.

### Phase 1 — Correct local sync, no delete (parallelizable only after 0.1 API shape is stable)

**Deliverable 1.1: Operand mapping with no trailing-slash magic.**
- Implement D3 forms: `--into`, `SOURCE... DIRECTORY` existing-dir convenience, exact `SOURCE DEST`, `--contents SOURCE DIRECTORY`. Trailing `/` bytes do not select contents mode. Path normalization may ignore a redundant trailing separator for lookup but must not change mapping; tests must prove `src/` and `src` map identically absent `--contents`.
- Proof: new ish-fs/command tests cover exact/into/existing-dir/contents/trailing-slash-equivalence/conflicting forms. `cargo test -p ish-fs` and targeted command tests pass. Threshold: all named cases pass, including the exact scenarios from baseline (`src` vs `src/` produce same result for each explicit form).

**Deliverable 1.2: Fresh sync and update sync for files/directories/symlinks.**
- Fresh destination absent: copies source content/tree. Existing destination: update changed entries, create missing entries, keep unchanged entries stable. Regular files/symlinks/dirs supported first; unsupported special objects diagnosed clearly (same stance as cp where practical).
- Proof: integration/unit tests: fresh copy, update changed file, preserve unchanged file bytes/identity where semantics promise, nested directories, symlink object preserved. `cargo test -p ish-fs -p ish` relevant tests pass. Threshold: all pass.

**Deliverable 1.3: Quick-check skip (incremental noop).**
- Unchanged regular files skipped by P2 rule. Verbose/dry-run reports skipped vs transferred. Noop run performs no data copy for unchanged files.
- Proof: test seeds destination, runs sync twice, asserts second run reports 0 transferred / N skipped (exact counters per implemented reporting) and destination unchanged; optionally assert bytes_streamed/cloned counters not increased for data transfer. `cargo test` targeted sync tests pass. Threshold: noop correctness + skip counters pass.

### Phase 2 — Deletion and hardening (after Phase 1)

**Deliverable 2.1: `--delete`.**
- Plan and remove destination-only entries within synchronized mapping, anchored and non-symlink-escaping. Dry-run shows deletions; verbose reports them.
- Proof: tests for delete absent-in-source file/dir, no delete without flag, delete with `--contents`, dry-run deletion report, refusal/safety for symlink escape cases. Threshold: all pass.

**Deliverable 2.2: Metadata/permission parity pass.**
- Match cp's representable metadata guarantees for synced regular files/directories/symlinks on supported platforms; diagnose unsupported required metadata without silent broadening.
- Proof: extend/port relevant ish-fs metadata tests to sync path where semantics overlap; platform-specific tests `#[cfg]` as existing tests do. Threshold: existing metadata-sensitive transfer tests still pass; new sync metadata tests pass on host platform.

### Phase 3 — Performance and proof (after correctness)

**Deliverable 3.1: Sync workloads in fs bench and measured comparison.**
- Add workloads: `sync-fresh-tree`, `sync-noop-tree`, `sync-update-small`, `sync-delete`, with fixed fixtures and correctness verification after each sample (like `verify_sample`/`fs_bench_verify` pattern). Compare ish `rsync` builtin path vs external rsync command selected by harness (`/usr/bin/rsync` or configured), recording implementation/version.
- Proof: benchmark harness commands in `.work/tmp` fixtures with perf-gate discipline; correctness verification passes for every accepted sample. `benches/fs/README.md`/results updated only when runs are valid (not busy/deferred) and labeled with host+rsync implementation.
- Threshold: correctness 100% on workload verifiers; median wall time and CPU meet or beat baseline rsync on fresh/noop workloads defined as in-scope, or any regression is explained by stronger guarantees and accepted by owner. Noisy-host rule: use existing perf-gate/interleaving/count policy; a deferred gate is reported, not counted.

**Deliverable 3.2: Docs and operator contract.**
- Update `README.md` native commands section, add `docs/filesystem.md` or `docs/rsync.md` reference section (follow existing docs style; topic map in `AGENTS.md`/`docs/` if needed), and builtin help text. Document non-goals (no remote, no trailing-slash magic, `-n` not dry-run) clearly.
- Proof: help output matches docs; doc examples use explicit forms. `cargo test` (doc-adjacent tests/help tests) pass. Threshold: reviewed text matches implemented behavior.

## Acceptance

Campaign is acceptable when:
1. D1-D5 owner decisions are recorded in this doc (answers filled in) or explicitly delegated.
2. `rsync --help` and invalid-option behavior work; builtin dispatch works in original process and is refused in pipeline/substitution contexts consistent with native cp/mv rules unless deliberately exempted and documented.
3. Operand mapping has no trailing-slash content magic; `--contents` is explicit; tests prove D3.
4. Fresh/update/noop sync is correct for supported object types; unchanged regular files are skipped by quick check with honest counters.
5. `--delete` is opt-in, correct, and dry-run visible.
6. Existing `cp`/`mv`/`ish-fs` tests remain green; new sync tests cover correctness and edge cases named above.
7. Performance comparison is measured with harness discipline for defined local workloads against a documented rsync implementation; results are recorded or explicitly deferred with "perf gate deferred: host busy" / unavailable-baseline rationale. Ad-hoc timings do not count as acceptance.
8. `README.md` and filesystem docs describe behavior and limits.

## Needs environment

- macOS and Linux CI/hosts used by ish for final acceptance. macOS 27 permission/clone/flag behaviors and Linux reflink/xattr/ACL differences need platform coverage (existing `ish-fs` test patterns).
- A reference `rsync` binary for benchmark comparison on each measured host (macOS has openrsync; Linux runners may have rsync 3.x). Record `rsync --version`/implementation in results. Do not claim parity with an unmeasured implementation.
- Perf-gate-ready host load conditions for P4 acceptance runs; otherwise report deferred and do not mark performance accepted.

## Execution boundary and non-goals

**May do (with normal repo workflow):** modify `crates/ish-fs`, `src/builtin.rs`, `src/fs_command.rs` or add `src/sync_command.rs`, `src/shell.rs`/dispatch plumbing, tests, benches, docs. Reuse existing dependencies/patterns. Commit coherent verified changes.

**Must not do:** add remote rsync protocol/ssh transport/daemon interop; add flag-compatible rsync CLI surface beyond approved aliases; run formatters; change unrelated cp/mv semantics; weaken existing safety/identity checks; claim "equal or greater performance" without a measured, labeled comparison.

**Non-goals:** rsync wire protocol versions, incremental rolling checksums over network, ACL/xattr fidelity beyond what ish-fs/platform already supports, Windows support, backup suffix rotation zoo, include/exclude/filter rule language, compression, bandwidth limits, daemon auth modules.

## Reference

- Native builtin registration/help: `src/builtin.rs:161-181,274-301,303-398`.
- Command parsing/rendering pattern: `src/fs_command.rs:1-654`.
- External/native dispatch context: `src/shell.rs:73-87,` and `rg -n "ExternalCommand::Copy|fs_command::run" src/shell.rs`; architecture `docs/architecture.md:6-22`.
- ish-fs API: `crates/ish-fs/src/lib.rs:24-308`.
- Preflight/policy: `crates/ish-fs/src/policy.rs:1-495`.
- Transfer engine: `crates/ish-fs/src/engine/mod.rs:1-679`, `crates/ish-fs/src/engine/transfer.rs:1-768`.
- Filesystem contract docs: `docs/filesystem.md`, `README.md:90-140`.
- Bench harness: `benches/fs/README.md`, `benches/fs/measure.sh`, `crates/ish-fs/examples/fs_bench.rs`.
- Baseline evidence (2026-11-07, ad-hoc): openrsync 2.6.9 compatible `/usr/bin/rsync` on macOS 27.2, fresh 0.085s/noop 0.031s for 201-file/1.78 MiB tree; `--delete` fresh real 0.08s/noop 0.02s; trailing slash creates `dst/src` vs contents-only.
