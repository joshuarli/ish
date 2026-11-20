# Shell and editor deslop evidence

Scope: ish `src/`, split between execution and persistence (DSLP-3/4/5) and editor and utilities (DSLP-6/11/16). Both are consumed by the ish binary and PTY tests. Measurements are Darwin arm64, default features, Rust 1.102.0-nightly (5b7b37aef 2026-11-08), base ffd27b855bd7262b4ee45e91673b0ba64f2508a9. Linux-specific branches are not measured here.

## Shell execution and persistence: types (DSLP-3)

`cargo clippy -p ish --lib --bins -- -W unsafe_code -W dead_code -W unreachable_pub -W clippy::wildcard_enum_match_arm -W clippy::match_wildcard_for_single_variants -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::unwrap_used -W clippy::expect_used -W clippy::redundant_clone -W clippy::needless_pass_by_value` exited 0. Counts are diagnostics with primary location in the component, not proven defects: unsafe 27, wildcard enum 3, single-variant wildcard 0, truncation 17, sign loss 2, unwrap 9, expect 33, redundant clone 0, needless pass by value 3. `unreachable_pub`: 5; see dead-code audit. Counts include separate lib and bin compilations where applicable. The component has 6,828 Rust lines and 306 matching lines under the deslop escape-hatch expression, including tests.

No candidate is adopted: no candidate has a demonstrated defect witness or removed escape hatch, and several flag intentional behavior. For example, `src/dispatch.rs:216` returns `None` for any non-static word, `src/history.rs:149` asserts the store just opened, and `src/signal.rs:387` writes a signal number to the signal pipe. These examples do not prove all remaining diagnostics false positives. Existing FFI makes `unsafe_code = forbid` inapplicable. No type contract or runtime code changed; escape-hatch count is unchanged.

## Shell execution and persistence: test inventory (DSLP-4)

| Contract | Guarding tests | Disposition |
| --- | --- | --- |
| CLI help, missing operands, command-mode exit and I/O | `tests/cli_help.rs:6`, `tests/login_shell.rs:9`, `:19`, `:25`, `:44`, `:52`, `:79` | Retain; literal output and process results. |
| Dispatch syntax and pre-effect refusals | `src/dispatch.rs:231`, `:277`, `:343`, `:358`; `tests/pty.rs:1588`, `:1616`, `:3124` | Retain; static and interactive policy boundaries. |
| Config set/alias parsing and bad lines | `src/config.rs:206`, `:213`; `tests/integration.rs:879`, `tests/pty.rs:3390` | Retain; malformed lines and loaded configuration. |
| History legacy import, journal corruption, append failure and concurrent writers | `src/history/persistence_tests.rs:78`, `:137`, `:261`, `:381`, `:400`, `:550`, `:715`, `:776`; `tests/history_import.rs:6`, `tests/pty.rs:3599`, `:3622`, `:3653`, `:3696` | Retain; persisted bytes and restart outcomes. |
| Suspended job ownership, resumption, signals and exit | `tests/pty.rs:4377`, `:4414`, `:4433`, `:4534`, `:4962`, `:4995`, `:5075` | Retain; terminal and process boundaries. |

The tests have unit/component, integration, and real PTY tiers. `make test` runs the default workspace including these layers; baseline costs and known transient failures are recorded in `TODO-DESLOP.md` and `AGENTS.md`. Focused edit checks can select a single test through `cargo test -p ish <name>`; a zero-test result is not evidence. Linux signal and filesystem behavior is not measured on this host. Needs environment: `cargo-llvm-cov` and `cargo-mutants` are unavailable, so branch coverage and bounded mutant survivor counts are unmeasured; this inventory is not a mutation pass. No boundary test has been added on inventory evidence alone.

A hand fault changed the parameter-operator refusal in `src/dispatch.rs` to `Ok(())`. `cargo test -p ish --lib dispatch::tests::undocumented_syntax_is_refused -- --exact` exited 101 on `${HOME:-x}`; after restoration it exited 0 (1 passed), and `cmp` confirmed byte-for-byte restoration. This is one demonstrated decision, not a substitute for the unavailable bounded mutant run.

## Shell execution and persistence: dead code (DSLP-5)

The strict scan above enabled `dead_code` and `unreachable_pub`; it reported no unsuppressed dead-code diagnostics in this component. The five `unreachable_pub` diagnostics are the methods on private `history::store::Store`: `open`, `snapshot`, `append`, `reset`, and `salvage` (`src/history/store.rs:91`, `:121`, `:130`, `:166`, `:185`). They are reachable internally through `History`; the effective visibility is already bounded by the private module. Retain all five as a single group of visibility-only warnings, not dead implementations. `src/history.rs:142`, `:196`, `:420`, and `:479` show calls; salvage is used in `src/history.rs`. The retained journal and migration paths are guarded by `src/history/persistence_tests.rs` and `tests/pty.rs`.

Debloat read: the builtin registry centralizes names and execution classes (`src/builtin.rs:84`, `:178`); the dispatcher enforces the documented finite policy (`src/dispatch.rs:64`); the history store owns locking and recovery (`src/history/store.rs:67`, `:76`); the shell owns the single suspended job (`docs/architecture.md:10`). These mechanisms have current callers and preserve documented behavior. No behavior-preserving cut was established in this component; no walk-back is proposed. This is a scoped read, not a complete structural sweep of every lens. Needs environment: `cargo-machete` is unavailable, so dependency liveness was not measured. No code was removed; macOS default features were scanned, and Linux-only paths were not compiled.

## Editor and utilities: types (DSLP-6)

The same strict Clippy command recorded above exited 0. Primary-location diagnostic counts for this component: unsafe 58, wildcard enum 24, single-variant wildcard 2, truncation 42, sign loss 17, unwrap 28, expect 16, redundant clone 1, needless pass by value 12. `unreachable_pub`: 9, examined as visibility leads rather than dead-code proof. The component has 20,405 Rust lines and 797 matching lines under the deslop escape-hatch expression, including tests. No candidate is adopted without a demonstrated witness or deleted escape hatch. The scan reports leads, not 209 established defects; for example, completion uses platform calls in `src/complete.rs:494`, and editor completion cancellation restores the saved line in `src/editor.rs:533`. Unsafe is required by current platform boundaries. No type contract or runtime code changed; escape-hatch count is unchanged.
