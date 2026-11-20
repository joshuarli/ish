# Integration tests, examples and workloads deslop evidence

Scope: `tests/` and `examples/`, `benches/`, `scripts/`; Darwin arm64, Rust 1.102.0-nightly (5b7b37aef 2026-11-08), base `150f8ac693757587d013324d190e764f46d23133`. Linux-only branches were not executed.

## Shell integration tests: types (DSLP-21)

`cargo clippy --message-format=short -p ish --all-targets -- -W unsafe_code -W dead_code -W unreachable_pub -W clippy::wildcard_enum_match_arm -W clippy::match_wildcard_for_single_variants -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::unwrap_used -W clippy::expect_used -W clippy::redundant_clone -W clippy::needless_pass_by_value` exited 0. Primary-location diagnostic counts in `tests/`, including repeated target compilations: unsafe 6, wildcard enum 0, single-variant wildcard 0, truncation 4, sign loss 0, unwrap 486, expect 68, redundant clone 0, needless pass by value 0, unreachable pub 23, dead code 0. These are leads, not established defects. Unwraps and expects assert setup and test results; `tests/common/mod.rs` and `tests/fdu_support.rs` are helper modules compiled into separate test crates. No candidate has a demonstrated defect witness or removes an escape hatch; no type or test code changed.

## Shell integration tests: test inventory (DSLP-22)

| Contract | Guarding tests | Disposition |
| --- | --- | --- |
| CLI help, command mode and argument errors | `tests/cli_help.rs:6`; `tests/login_shell.rs:9`, `:19`, `:25`, `:44`, `:52`, `:79` | Retain; output, status, streams and refusals. |
| Interactive execution, unsupported syntax, job control and signals | `tests/pty.rs:987`, `:1144`, `:1256`, `:4377`, `:4962`; `tests/external_signals.rs:54` | Retain; real terminal and process boundaries. |
| Persistence, recovery, concurrent writers and import refusal | `tests/pty.rs:3584`, `:3599`, `:3622`, `:3653`, `:3696`; `tests/history_import.rs:6` | Retain; restart, failure and unchanged files. |
| History selection and pasted query | `tests/pty.rs:2025`, `:2073`, `:3800`; `tests/history_search_paste.rs:70`, `:91`, `:111` | Retain; query editing, cancellation and session behavior. |
| Prompt identity, Git refresh and terminal presentation | `tests/prompt_identity.rs:4`, `:14`; `tests/prompt_git_refresh.rs:78`, `:94`; `tests/pty.rs:1163`, `:3407` | Retain; sanitization, refresh and display. |
| fdu interaction, deletion and screen snapshots | `tests/fdu_pty.rs:10`, `:49`, `:74`, `:106`; `tests/fdu_snapshots.rs:121`, `:128`, `:188` | Retain; terminal restoration, deletion policy and snapshots. |
| fz worker framing and interactive routing | `tests/fz_worker.rs:20`, `:34`, `:44`, `:57`; `tests/pty.rs:5561`, `:5585`, `:5643` | Retain; bytes, errors, routing and terminal ownership. |
| Environment isolation and signals | `tests/denv_environment.rs:48`, `:105`, `:123`; `tests/external_signals.rs:54` | Retain; state restoration and child signal disposition. |

Needs environment: `cargo-llvm-cov` and `cargo-mutants` were not found; branch coverage and bounded mutation survivors are unmeasured. Linux-only `tests/pty.rs:1121` is unexecuted on Darwin. This inventory is not a mutation pass.

Hand fault: in `scripts/import-fish-history:15`, replace `if [[ -e "$ish_log" ]]; then` with `if [[ ! -e "$ish_log" ]]; then`, reversing the migration refusal. `cargo test -p ish --test history_import legacy_fish_import_refuses_existing_log_without_changing_files -- --exact` exited 101: `legacy_fish_import_refuses_existing_log_without_changing_files` failed with `legacy importer reported success after migration`. After copying back the saved original, `cmp scripts/import-fish-history .work/tmp/import-fish-history.original` exited 0, and the same test exited 0 (1 passed). This witnesses one destructive-import decision, not a bounded mutation run.

## Shell integration tests: dead code (DSLP-23)

The strict scan reported no dead-code diagnostics. Its 23 unreachable-public diagnostics are visibility leads in test helper modules, including `tests/common/mod.rs:4` and `tests/fdu_support.rs:11`, not proof of dead code. `tests/fdu_snapshots.rs:23` and `:121`–`:198` reference all nine `tests/snapshots/*.ptytest` files by name. `tests/fdu_support.rs:231` calls the layout fixture generator; test targets and shared helpers are compiled by Cargo. Workspace, Makefile and `.github/` searches did not establish an unused helper or fixture safe to remove. Needs environment: `cargo-machete` was not found; dependency liveness is unmeasured. No code removed.

## Examples and workloads: types (DSLP-24)

The strict Clippy command above also covers ish examples and benches. Primary-location counts in `examples/`: unsafe 3, wildcard enum 11, single-variant wildcard 0, truncation 39, sign loss 0, unwrap 2, expect 11, redundant clone 2, needless pass by value 3, unreachable pub 0, dead code 0. In `benches/`: unsafe 10, wildcard enum 3, single-variant wildcard 0, truncation 1, sign loss 3, unwrap 59, expect 105, redundant clone 0, needless pass by value 1, unreachable pub 0, dead code 0. Counts include repeated target compilations and the filesystem benchmark test target; scripts have no Rust Clippy targets. No candidate is adopted without a defect witness or removed escape hatch; these diagnostics alone do not establish defects.

## Examples and workloads: test inventory (DSLP-25)

| Contract | Guarding tests | Disposition |
| --- | --- | --- |
| Semantic bridge comparator and minimization | `examples/semantic_bridge/runner.rs:825`, `:835`, `:844`, `:858`; `examples/semantic_bridge/evaluation.rs:1584`, `:1604` | Retain; semantic outcomes, invalid outcomes, shrinking and trace projection. |
| Mutation runner timeout and executable selection | `scripts/test-semantic-mutations.py:23`, `:45`; `Makefile:43` | Retain; process cleanup and explicit executable. |
| Legacy import refusal | `tests/history_import.rs:6` | Retain; fault witness above. |
| Benchmark harness and fixtures | `benches/fs/fs_bench.rs:31`, `:45`, `:59`, `:74`; `tests/fdu_pty.rs:189` | Retain; filesystem sample preparation, verification and cleanup, plus layout workload. |

`Makefile:96` wires `make bench` to `cargo bench --bench bench` and `benches/baseline.json`; `Cargo.toml:44` declares the bench and `:49` the filesystem test target. `cargo check --workspace --all-targets` checks their compilation; it does not execute a benchmark or validate baseline performance. Needs environment: `cargo-llvm-cov` and `cargo-mutants` are unavailable. `benches/bench.rs:9` Linux-specific paths and `benches/fs/native-fixtures.sh:11` Linux mount fixtures were not run on Darwin. APFS raw filename bridge limitations are recorded in DSLP-30.

## Examples and workloads: dead code (DSLP-26)

Workspace, Makefile and `.github/` searches found the bridge invoked by `Makefile:39`, semantic mutation and refinement scripts by `Makefile:43`–`:47`, the fz importer and its test documented in `docs/fuzzy-pickers.md:133`, `:146`, and filesystem runners by `Makefile:27` and `benches/fs/bench.sh:101`. `docs/filesystem-testing.md:44` documents the native fixture entrypoint. The independent `benches/fs/bench-sync.sh` and `smoke-sync.sh` have no in-repository callers found; direct invocation cannot be ruled out, so their liveness is unresolved and they are retained. No unused entrypoint was proven safe to delete. Needs environment: `cargo-machete` was not found; dependency liveness is unmeasured.
