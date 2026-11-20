# Filesystem and fdu deslop evidence

Scope: `crates/ish-fs/` (`ish-fs`, DSLP-9/14/19) and `crates/fdu-*/` (`fdu-core`, `fdu-posix`, `fdu-delete`, `fdu-scan`, `fdu-tui`, DSLP-10/15/20). Consumers: `ish` (`src/fs.rs`, `src/sync_command.rs`, `src/rmrf.rs`, `src/fdu.rs`, `src/fdu_browser.rs`), workspace integration tests, and benchmarks. Measurements are Darwin arm64, default features, Rust 1.102.0-nightly (5b7b37aef 2026-11-08), base `9bcbae4eb3bf6db8614d6768fc9d22f992e7d4a8`. Linux-specific branches and tests are not compiled or measured here.

## Filesystem engine: types (DSLP-9)

`cargo clippy -p ish-fs --all-targets --message-format=short -- -W unsafe_code -W dead_code -W unreachable_pub -W clippy::wildcard_enum_match_arm -W clippy::match_wildcard_for_single_variants -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::unwrap_used -W clippy::expect_used -W clippy::redundant_clone -W clippy::needless_pass_by_value` exited 0. Emitted primary-location counts across all targets (including duplicate lib and integration-test compilations of shared modules): unsafe 94 (59 unique), wildcard enum 15 (15 unique), single-variant wildcard 0, truncation 32 (21 unique), sign loss 17 (11 unique), unwrap 1,084 (1,080 unique), expect 365 (354 unique), redundant clone 15 (15 unique), needless pass by value 9 (9 unique). `unreachable_pub`: 35 (24 unique); `dead_code`: 0; see dead-code audit. `crates/ish-fs/` has 17,620 Rust lines and 1,796 matching lines under the deslop escape-hatch expression, including tests and examples.

No candidate is adopted: no candidate has a demonstrated defect witness or removed escape hatch. Platform FFI in `crates/ish-fs/src/platform/macos.rs` (`acl_*`, `copyfile`, `fclonefileat`, and resource-fork helpers) requires `unsafe`; `unwrap`/`expect` cluster in tests and `Mutex::lock()` calls; integer casts in `crates/ish-fs/src/roots.rs:41`, `crates/ish-fs/src/platform/macos.rs:401`, and `crates/ish-fs/src/sync.rs:1073`, `:1145` convert bounded POSIX `stat` fields. No type contract or runtime code changed; escape-hatch count is unchanged.

## Filesystem engine: test inventory (DSLP-14)

| Contract | Guarding tests | Disposition |
| --- | --- | --- |
| Preflight, collision, merge, `--into` basename, and ancestry policy | `crates/ish-fs/tests/policy.rs:84`, `:126`, `:160`, `:178`, `:219`, `:237`, `:257`, `:296`, `:320`, `:342`, `:392`, `:418`, `:438`, `:462`, `:667`; `crates/ish-fs/src/lib.rs:244`, `:281` | Retain; logical policy matrix, alias rejection, and dry-run invariants. |
| Transfer, atomic replacement, hardlinks, metadata/ACL/non-UTF-8 preservation, and SIGKILL safety | `crates/ish-fs/tests/transfer.rs:39`–`:236`; `crates/ish-fs/tests/preservation.rs:124`–`:336`; `crates/ish-fs/tests/publication.rs:69`, `:103`, `:150`; `crates/ish-fs/tests/interruption.rs:118` | Retain; byte-exact copies, concurrent reader atomicity, and staging cleanup. |
| Move safety and resource/job budgets | `crates/ish-fs/tests/move_safety.rs:27`–`:68`; `crates/ish-fs/tests/resources.rs:125`, `:205`, `:267`, `:304`; `crates/ish-fs/src/lib.rs:293`, `:304` | Retain; same-FS directory renames and `Jobs`/descriptor/memory bounds. |
| Darwin clonefile, sparse copy, resource fork, and staging boundary | `crates/ish-fs/tests/native_macos.rs:128`–`:1065` (1 ignored root-owned fixture at `:563`) | Retain; kernel acceleration, fallback, and umask boundaries. |
| `sync::execute` (`Exact`/`Into`/`Contents`), delete, dry-run, and traversal safety | `crates/ish-fs/tests/sync_adversarial.rs:18`–`:545`; `tests/integration.rs:2047`+ | Retain; permission, broken-symlink, overlap, and cancellation cleanup. |
| Staging journal recovery and record spool | `crates/ish-fs/src/engine/quarantine_tests.rs:50`–`:211`; `crates/ish-fs/src/engine/repair_tests.rs:63`–`:830`; `crates/ish-fs/src/engine/records.rs:531`–`:621` | Retain; crash-boundary recovery and binary spool integrity. |

`cargo test -p ish-fs --quiet` exited 0 (lib 92 passed, 1 ignored; `interruption` 1 passed, 1 ignored; `move_safety` 6 passed; `native_macos` 26 passed, 1 ignored; `policy` 15 passed; `preservation` 5 passed; `publication` 3 passed; `resources` 1 passed; `sync_adversarial` 11 passed; `transfer` 13 passed; `native_boundaries` and `native_linux` 0 tests on Darwin). Linux-only and `#[ignore]`d cross-device, `FICLONE`, immutable-flag, and root-owned fixtures are not run on this host. Needs environment: `cargo-llvm-cov` and `cargo-mutants` are unavailable, so branch coverage and bounded mutant survivor counts are unmeasured. No new boundary test was added on inventory evidence alone.

A hand fault changed `Action::Skip` to `Action::Replace` in `crates/ish-fs/src/policy.rs:159`. `cargo test -p ish-fs --test policy generated_top_level_policy_matrix_matches_the_logical_model -- --exact` exited 101 at `crates/ish-fs/tests/policy.rs:575:23`; after restoration it exited 0 (1 passed), and `cmp -s` confirmed byte-for-byte restoration. This confirms one decision, not a full mutation sweep.

## Filesystem engine: dead code (DSLP-19)

The strict scan enabled `dead_code` and `unreachable_pub`: 0 `dead_code` diagnostics and 24 unique `unreachable_pub` diagnostics (35 raw across targets): 11 on `roots::Observation`/`roots::Root` methods (`crates/ish-fs/src/roots.rs:31`–`:180` inside `pub(crate) mod roots`), 11 on `engine::records::Records` methods (`crates/ish-fs/src/engine/records.rs:49`–`:505` inside private `mod engine`), and 1 on `make_fifo` (`crates/ish-fs/tests/support/mod.rs:4`). All have active callers within `ish-fs` or its integration tests and are retained as visibility-only warnings.

Existing `#[allow(dead_code)]` attributes in `crates/ish-fs/src/platform/linux.rs:72`, `:78`, `:84`, `:91` guard `#[cfg(test)]` fault hooks called by `crates/ish-fs/src/engine/tests.rs:90`–`:219` and `crates/ish-fs/tests/native_linux.rs:176`–`:287`; integration tests `native_boundaries.rs`, `native_macos.rs`, and `native_linux.rs` suppress unused helpers from `include!`/`#[path]` internal modules. `sync::owner_lacks_directory_write` (`crates/ish-fs/src/sync.rs:67`) is called at `:615` and retained as part of the public `ish_fs::sync` API. Needs environment: `cargo-machete` and `cargo-udeps` are unavailable, so automated dependency liveness was not measured; manual inspection confirms `fdu-posix`, `rustix`, `libc`, and dev-dependency `rustc-hash` (`crates/ish-fs/src/engine/dry_run.rs:151`) are all used. No code or dependency was removed.

Debloat read: `policy` (`crates/ish-fs/src/policy.rs`) owns preflight and alias/overlap checks, `roots` (`crates/ish-fs/src/roots.rs`) anchors directory-relative identities, `engine` (`crates/ish-fs/src/engine/mod.rs`) owns staged transfer and quarantine repair, and `sync` (`crates/ish-fs/src/sync.rs`) owns incremental synchronization. All have active workspace consumers and guard documented invariants; no behavior-preserving cut is proposed.

## fdu components: types (DSLP-10)

The same strict `cargo clippy -p <crate> --all-targets --message-format=short` command exited 0 for all five `fdu-*` crates (no duplicate diagnostic locations across targets):

| Crate | Lines | Escape hatches | Unsafe | Wildcard enum | Single-var wildcard | Truncation | Sign loss | Unwrap | Expect | Redundant clone | Needless by-val | `unreachable_pub` | `dead_code` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `fdu-core` | 921 | 49 | 0 | 0 | 0 | 2 | 0 | 43 | 5 | 0 | 0 | 0 | 0 |
| `fdu-posix` | 1,754 | 213 | 9 | 1 | 0 | 0 | 2 | 182 | 4 | 0 | 2 | 0 | 0 |
| `fdu-delete` | 1,920 | 203 | 0 | 1 | 0 | 3 | 2 | 170 | 40 | 0 | 5 | 0 | 0 |
| `fdu-scan` | 2,450 | 21 | 1 | 1 | 0 | 1 | 0 | 6 | 12 | 0 | 4 | 0 | 0 |
| `fdu-tui` | 2,609 | 49 | 2 | 8 | 1 | 12 | 0 | 42 | 2 | 0 | 3 | 18 | 0 |

Total across `fdu-*`: 9,654 Rust lines and 535 matching escape-hatch lines, including tests. No candidate is adopted: no candidate has a demonstrated defect witness or removed escape hatch. `unsafe` is confined to Darwin FFI (`crates/fdu-posix/src/identity.rs:101`, `crates/fdu-posix/src/quarantine.rs:548`–`:578`, `crates/fdu-scan/src/macos.rs:163`), terminal termios setup (`crates/fdu-tui/src/terminal.rs:159`), and `str::from_utf8_unchecked` after `valid_up_to()` (`crates/fdu-tui/src/lib.rs:827`); truncation and sign casts reflect terminal `u16` cell bounds (`crates/fdu-tui/src/screen.rs`, `src/lib.rs`), `f_mntonname` bytes (`crates/fdu-posix/src/identity.rs:106`), and POSIX `stat` sizes (`crates/fdu-posix/src/quarantine.rs:162`, `crates/fdu-delete/src/lib.rs:1062`). No type contract or runtime code changed; escape-hatch counts are unchanged.

## fdu components: test inventory (DSLP-15)

| Crate / contract | Guarding tests | Disposition |
| --- | --- | --- |
| `fdu-core`: `Tree`, `NodeId`, `ModifiedTime`, `fold_entry_batch`, `finish_scan_directory`, tombstones, overflow | `crates/fdu-core/src/lib.rs:675`, `:707`, `:749`, `:859`, `:898` | Retain; arena IDs, totals propagation, and batch folding. |
| `fdu-posix`: anchored identity, `Session` quarantine, lock conflict, umask rejection, crash recovery | `crates/fdu-posix/src/quarantine.rs:1101`–`:1207`; `crates/fdu-posix/tests/recovery.rs:79`–`:372` (2 `#[ignore]`d subprocess fixtures `:33`, `:56`; 1 `#[ignore]`d bind-mount test `:324`) | Retain; killed-child restoration and tampered-record rejection. |
| `fdu-delete`: `create_plan`, `create_browser_plan`, quarantine/direct deletion, sync error reporting | `crates/fdu-delete/src/lib.rs:1134`, `:1158`; `crates/fdu-delete/tests/browser_plan.rs:30`–`:178`; `crates/fdu-delete/tests/quarantine.rs:76`–`:422` | Retain; selective deletion, exclusion skipping, and journal validation. |
| `fdu-scan`: `ScanExclusions`, `open_root_no_follow`, `start_indexed_scan_with_exclusions` | `crates/fdu-scan/src/exclusions.rs:105`; `crates/fdu-scan/tests/indexed_policy.rs:200`, `:216` | Retain; root-relative exclusion matching and indexed scan order. |
| `fdu-tui`: `TerminalSession` / `render`, `LsColors`, `Screen` diffing, input decoding, formatting | `crates/fdu-tui/src/lib.rs:877`–`:1352`; `crates/fdu-tui/src/lscolors.rs:230`–`:305`; `crates/fdu-tui/src/screen.rs:350`; `crates/fdu-tui/src/terminal.rs:456`–`:552` | Retain; rendering, width clipping, key decoding, and ANSI diffing. |

`cargo test -p <crate> --quiet` exited 0 on all five crates (`fdu-core` 5 passed; `fdu-posix` 5 lib passed + 8 integration passed, 2 ignored; `fdu-delete` 2 lib + 8 `browser_plan` + 16 `quarantine` passed; `fdu-scan` 1 lib + 2 `indexed_policy` passed; `fdu-tui` 37 passed). End-to-end `fdu` integration also lives in `src/fdu_browser.rs`, `src/rmrf.rs`, `tests/fdu_snapshots.rs`, and `tests/fdu_pty.rs`. Needs environment: `cargo-llvm-cov` and `cargo-mutants` are unavailable, so branch coverage and bounded mutant survivor counts are unmeasured. No new boundary test was added on inventory evidence alone.

A hand fault changed `if record.state == NodeState::Complete` to `if matches!(record.state, NodeState::Complete | NodeState::Incomplete)` in `crates/fdu-delete/src/lib.rs:290`. `cargo test -p fdu-delete --test browser_plan browser_scan_errors_inside_a_directory_skip_only_that_entry -- --exact` exited 101 at `crates/fdu-delete/tests/browser_plan.rs:46:5`; after restoration it exited 0 (1 passed), and `cmp -s` confirmed byte-for-byte restoration.

## fdu components: dead code (DSLP-20)

The strict scan enabled `dead_code` and `unreachable_pub`: 0 `dead_code` diagnostics across all five `fdu-*` crates and 18 `unreachable_pub` diagnostics in `crates/fdu-tui/src/screen.rs:14`–`:331` inside private `mod screen`, retained as visibility-only warnings on items used by `crates/fdu-tui/src/lib.rs`.

Workspace consumers (`src/fdu.rs`, `src/fdu_browser.rs`, `src/rmrf.rs`, `crates/ish-fs/`) consume all five crates; self-only public items (`fdu_core::BatchFoldOutcome`, `fdu_core::NodeRecord::name_range`, `fdu_core::Tree::mark_stale_to_root`, `fdu_delete::PlanError`, `fdu_delete::SkippedEntry`, `fdu_tui::SPLIT_MIN_COLUMNS`, `fdu_tui::STACK_MIN_ROWS`, `fdu_tui::LsColors::from_env`) are part of their library contracts and retained. `#[allow(dead_code)]` on `IndexedResult` fields in `crates/fdu-scan/tests/indexed_policy.rs:47`, `:49` is test-fixture bookkeeping. Needs environment: `cargo-machete` and `cargo-udeps` are unavailable, so dependency liveness was not measured; manual inspection confirms every declared dependency across `crates/fdu-*/Cargo.toml` is used. No code or dependency was removed.

Debloat read: the five crates separate filesystem-independent tree state (`fdu-core`), descriptor-anchored identity and crash-recoverable quarantine (`fdu-posix`), parallel traversal (`fdu-scan`), postorder/browser deletion planning and execution (`fdu-delete`), and terminal rendering (`fdu-tui`). Each boundary has active workspace callers and distinct ownership; no behavior-preserving cut is proposed.
