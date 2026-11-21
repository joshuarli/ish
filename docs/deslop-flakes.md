# Flake investigation

Darwin arm64, base `54832d69ad35db8791954d6d496f03d28de652a7`. The baseline command was `bash .work/tmp/flake-loops.sh`, with `TMPDIR="$PWD/.work/tmp"`. Its contents for the second run follow; the first run used `{1..8}` burners, `{1..3}` iterations and `${test}-${i}.log` for the log name. Each command ran exactly one named test.

```bash
#!/bin/bash
export TMPDIR="$PWD/.work/tmp"
pids=()
for i in {1..16}; do yes > /dev/null & pids+=("$!"); done
(while :; do /usr/bin/true; done) & pids+=("$!")
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null' EXIT
for test in input::tests::replies_between_typed_keys_leave_the_typing_intact input::tests::reply_and_report_sequences_are_consumed_whole_at_every_split builtin_lookup_completion_and_explicit_paths_share_the_policy completion_resize_rerenders_grid process_picker_cancel_no_match_and_explicit_term_kill_acceptance denv::tests::retired_helper_flood_respects_the_evaluation_deadline; do
  case "$test" in *::*) target=--lib;; *) target='--test pty';; esac
  failed=0
  for i in {1..10}; do
    cargo test -p ish $target "$test" -- --exact --test-threads=1 > ".work/tmp/stress-${test}-${i}.log" 2>&1 || failed=$((failed+1))
    printf '%s iteration=%s failures=%s\n' "$test" "$i" "$failed"
  done
done
```

## DSLP-27

Reproduced and fixed. Isolated runs never failed, so the earlier loops (above) could not see it: the test binary has to run the whole library suite, or at least the epsh redirect test below, next to the decoder tests. The failure is the 20-second `decoder must reach the sentinel instead of stalling: Timeout` panic in `decode_parts` (`src/input.rs:1107`), which only `reply_and_report_sequences_are_consumed_whole_at_every_split` and the other `decode_parts` tests can raise.

**Evidence.** The base test binary (`cargo test -p ish --lib --no-run`) was run directly with `TMPDIR="$PWD/.work/tmp"`:

- `input::` alone under sixteen `yes` burners: 15/15 passed, 5.1 s each. `--skip denv::` (whole suite, 8 burners): 8/8 passed in about 6 s. `input:: denv::`: 10/10 passed (without extra decoder load).
- The whole suite under sixteen burners (the loop in the DSLP-33 section, without `--test-threads=1`) failed the decoder test in roughly one run in three, exit 101, in 29 to 40 s. Other lanes were also loading the host (load average 30 to 50).
- Temporary diagnostics in `decode_parts` (removed; the patch is not kept) showed the same state every time: the writer had written the sentinel `Z`, the reader thread was blocked in `poll(2)` called from `InputReader::read_event` (`sample` showed it in `poll`, not spinning), a zero-timeout `poll` of the same pipe fd reported `POLLIN`, the signal fd was quiet, and a second write of `Z` did not wake the reader either. The reader had already consumed the first part. The stalled splits were always the first allowed split of a sequence (two bytes: `ESC [`, `ESC _`, `ESC O`).
- A 500 ms sliced `poll` in place of `poll(-1)` removed the stalls (0/14 loaded runs, none of the fourteen printed a timed-out poll with data readable), which pointed at the wait rather than the decoder.

**Cause (established).** `denv::tests::retired_helpers_cannot_redirect_failure_channel_epsh` runs the in-process epsh evaluator on `source_env shared N>/dev/null` for N in 3 to 9 and for the failure-channel descriptors from 10 up. epsh applies a redirection by `dup2` onto descriptor N in the test process and restores it afterwards (`crates/epsh/src/redirect.rs:91`, `crates/epsh/src/redirect.rs:266`). Every test runs in one process with one descriptor table, so any pipe another test thread holds at one of those numbers is replaced by `/dev/null` for the window. On macOS a thread already sleeping in `poll` on that descriptor is never woken again, even after the pipe is restored and written. The decoder tests make about 100 short-lived pipe pairs, so they are the ones that hit the numbers. The same applies to any test thread that holds a descriptor in that range.

Deterministic reproduction, no test suite involved: a thread blocks in `poll(-1)` on a pipe's read end, the main thread waits 100 ms, then does what epsh does (`fcntl(F_DUPFD, 100)` to save the descriptor, `dup2` `/dev/null` over it, `dup2` the saved descriptor back, close the saved copy) and writes one byte. Compiled with `rustc -O dup.rs`, three runs printed `reader STALLED: poll sleeping despite data` (exit 0 each). The source was scratch (`.work/tmp/pollrace/dup.rs`, 40 lines of `extern "C"` declarations) and is not kept.

Reproduction in the suite, base `a9b1966f`, no burners: `$BIN input:: denv::tests::retired_helpers_cannot_redirect_failure_channel_epsh` hung or failed in 2 of 3 runs (`timeout 60` exit 124 twice, the passing run took 5.1 s).

**Fix.** The epsh variant of the redirect test now re-executes the test binary and runs the redirections in the child, as the 64-descriptor and low-descriptor tests already do (`src/denv.rs:2046`). No product code changed and no deadline changed. The Bash variant stays in process: its `dup2` calls run in the forked child (`pre_exec`). After the change the same two-filter command passed 10/10 in 5.3 s each, exit 0. The loaded whole-suite loop with sixteen burners (the base-binary loop's command, fixed binary, 14 runs, all exit 101 from the denv deadline failures below) produced no `input::` failure; at the base rate of about one in three, 0/14 has probability below 1%.

`replies_between_typed_keys_leave_the_typing_intact` shares `decode_parts` and therefore the same exposure; it was not caught by name in these runs.

Not caused by this: the denv tests that evaluate `.envrc` files under a 2 s deadline (`retired_helpers_invalidate_masked_and_forked_candidates`, `dotenv_if_exists_loads_approved_values_during_evaluation`, `evaluators_preserve_helper_rejection_with_a_64_descriptor_limit`, `bash_snapshot_writers_survive_exec_when_low_fds_are_free`, `bash_requires_a_completed_candidate_before_applying_exports`, `retired_helper_flood_respects_the_evaluation_deadline`) failed in most loaded whole-suite runs with `.envrc evaluation timed out after 2s`. That is the DSLP-33 class (Bash startup against a test-chosen deadline) at a load average of 30 to 50.

## DSLP-28

Cause plausible, not witnessed in a natural failure. Neither test failed in 30 isolated runs under sixteen burners plus process churn (`.work/tmp/loop.sh`: `pty-<hash>` binary, `--exact --test-threads=1`) or in eight full-suite runs under the same load before the change (`.work/tmp/suite.sh`, default test threads).

**Cause (harness, deterministic).** Both tests pressed Tab, called `wait_for_quiescence(100)` or `(200)`, and asserted on the screen. `PtyShell::wait_for_quiescence` passes its timeout as both the overall and the quiet bound and discards the result, so it is a pause of at most that length, not a wait for the render. If the shell handles Tab later, the assertion runs on the old screen. The Escape wait also required `screen != before_escape`, which can never hold when `before_escape` predates the grid: closing the grid then returns the screen to that snapshot and the wait times out after 2 s, the ticket's symptom. A natural trigger needs the shell to take longer than 100 to 200 ms to draw one completion; DSLP-35 measured shell startup under load at p50 0.46 s and a maximum of 4.8 s with 40 concurrent shells.

**Reproduction.** A test-chosen stall stands in for the load: `stall_shell` sends SIGSTOP to the shell and SIGCONT 400 ms later, and Tab is sent in between. With the old waits and that stall, both tests failed in the one run made of each (`expected "aaa.txt" once in screen` for the resize test; an assertion on the completion text for the lookup test).

**Fix.** The tests wait for the grid (all three names once) and for the completed `echo`, then for the grid at the new size after the resize, then for the grid to close. The refused-builtin check types `x` after Tab and asserts only once `exex` is on the prompt, because input is handled in order and absence cannot be waited for directly. Both tests keep the 400 ms stall, so they now pin latency tolerance. After the change the two tests passed 24/24 isolated runs and the lookup test a further 40/40 under sixteen burners. In eight post-change full-suite runs the completion waits never failed, but the lookup test timed out once (and once under `cargo test` with two tests) at the 5 s prompt wait after `./bin/exec`, an external script that runs after the completion section; that is the external-command latency listed under DSLP-35. Other tests share the `screen != before_escape` conjunct, but each first waits for its overlay, so the conjunct is redundant there and was left.

## DSLP-32

Fixed (product). Under sixteen burners plus process churn the test failed 1/10 (earlier research) and 1/30 (this session; `pk: process lookup failed: Input/output error (os error 5)`, log `loop-process_picker_...-7.log`) isolated, and once in eight full-suite runs (timeout waiting for `selected]`; its log was overwritten). The ticket's `Invalid argument (os error 22)` and `process changed during snapshot` are the same defect.

**Cause (product).** `pk` lists every process of the user and binds each to a handle. On macOS `bind` (`src/pk/macos.rs`) re-read the metadata and returned any failure, or a changed record, as an ordinary error, which `snapshot` (`src/pk.rs`) treats as fatal for the whole lookup. Any short-lived process on the host, not the test fixture, that exits or execs between the first read and the bind aborts it. The first read already tolerates the same failures by counting the process as not inspectable. A probe (`pk::macos::churn_probe`, removed) ran eight threads spawning `/usr/bin/true` while repeatedly listing, reading, and binding every owned process for 20 s:

```
   72297 bind ok
       1 bind None "process changed during snapshot"
       1 bind Some(22) Invalid argument (os error 22)
   32951 first Some(1) Operation not permitted
    5516 first Some(3) No such process
     464 first Some(2) No such file or directory
       6 first Some(22) Invalid argument
       3 first Some(5) Input/output error
```

EIO and EINVAL come from the argument and path reads of a process that is exiting or executing. A zombie reads as ESRCH, which `gone` already skips, so it does not reproduce them.

**Fix.** `bind` now reports a process that changed or became unreadable as `NotFound`, which `snapshot` skips like an exited process; the Linux pidfd bind reports an identity change the same way (compile-checked for `x86_64-unknown-linux-musl`, not run). Resource errors such as EMFILE stay fatal (`resource_failure_is_not_an_empty_snapshot_or_pid_fallback`). `bind_skips_a_process_whose_identity_changed_since_listing` and `bind_skips_a_process_that_became_unreadable_since_listing` (`confirm` with EIO and EINVAL) failed before the change (`kind: Other` and `errno 5`) and pass after. The same probe after the change: 132582 binds, 4 skipped as gone, none fatal. The picker test passed 40/40 runs under sixteen burners plus churn; the earlier failure rate is below what that count resolves, so the unit tests and the probe are the evidence.

## DSLP-33

Reproduced but unresolved. The test passed 3/3 runs at the base commit under eight burners and failed 1/10 runs against unchanged base source under sixteen burners, with process churn in both runs. The failure was `.envrc evaluation timed out after 0.2s` at `src/denv.rs:2181`. A temporary assertion diagnostic identifying the evaluator reproduced it on iteration 13 of this command and printed `bash=true`; the preceding 12 passed:

```bash
export TMPDIR="$PWD/.work/tmp"
pids=(); for i in {1..16}; do yes > /dev/null & pids+=("$!"); done
(while :; do /usr/bin/true; done) & pids+=("$!")
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null' EXIT
for i in {1..30}; do cargo test -p ish --lib denv::tests::retired_helper_flood_respects_the_evaluation_deadline -- --exact --test-threads=1 > .work/tmp/denv-diagnostic.log 2>&1 || { printf 'failure iteration %s\n' "$i"; cat .work/tmp/denv-diagnostic.log; break; }; printf 'passed %s\n' "$i"; done
```

The Bash path starts its deadline after spawn (`src/denv.rs:1221`) and checks its failure channel before checking that deadline (`src/denv.rs:1231`). This does not establish whether Bash reached the helper before the deadline or why the record was absent. A temporary source-start file marker under the same load passed 200/200 iterations and therefore did not resolve that question. Both probes were removed. No fix or post-fix loop was attempted; increasing the test deadline would be unsupported by this evidence.

**Cause (established).** The test passes a 200 ms evaluation deadline to `eval_env_bash`. That deadline starts just after spawn (`src/denv.rs:1221`), so it must cover Bash startup, the helper prelude, and the first `dotenv` failure record. Bash startup alone exceeded it under the same load: `python3 -I .work/tmp/bash-startup.py` timed 400 `bash -e -c :` runs under sixteen `yes` burners on this 18-core host at p50 29.6 ms, p99 88.6 ms, max 986.6 ms, one over 200 ms (idle: max 36.2 ms). A deterministic reproduction puts a `bash` wrapper first on `PATH` that runs `sleep 0.25` and then `exec /bin/bash "$@"`. With it, base fails every time with the ticket's error, `.envrc evaluation timed out after 0.2s`, and the fixed test passes.

**Fix.** The evaluation timeout is now 2 s, the elapsed bound is that timeout plus 1 s, and the outer watchdog is 10 s. The test still requires the `dotenv` diagnostic from both evaluators and still bounds the flood. The passing case returns on the first failure record, so the longer deadline does not slow it. No product code changed. Loaded loops in one session (sixteen burners plus process churn) showed base 0/30 and fixed 0/30, so the base failure rate is below what 30 iterations resolve; the delayed-Bash reproduction is the decisive evidence.

## DSLP-34

Partly fixed. The CI failure (run 44822681805, attempt 1, macOS job) froze `filter-prompt-100x24` mid-scan: the status read `24971 entries…` and the header `24971 total`, against `192238` in the golden. fdu only rescans on an explicit refresh (`src/fdu_browser.rs:870`), so `wait_ready` accepted a screen during the first scan. Under ten `yes` burners, `cargo test -q --test fdu_pty --test fdu_snapshots` reproduced it once in 27 runs (`marked_pane_beside_the_list`, frozen at `20175 entries…`).

**Cause.** fdu writes each frame as a cell diff (`crates/fdu-tui/src/lib.rs:210`), and ptytest's vt100 screen has no frame boundary: it shows whatever bytes have arrived. A full-screen diff is larger than the pty buffer. When the reader drains one chunk and fdu is descheduled before writing the next, the screen holds a partial frame. When the count gains a digit, the diff shifts `entries…` one column right, so a write torn after `entries` leaves a stale space in the next cell. The status then matches the ready predicate (`<count> entries `). `Session::wait` also returned whatever followed 25 ms of quiet without re-checking its predicate, while fdu redraws only every 100 ms during a scan (`SCAN_REDRAW_INTERVAL`). Under the same load, `help_screen_opens_and_closes` failed 3 times in 60 runs on a settled screen that showed `Tab switches` without `there is no Trash`, which belongs to the same frame. A torn frame can therefore stay quiet for longer than 25 ms.

**Fix.** `wait_settled` (`tests/fdu_support.rs`) loops until the predicate holds on a settled screen. `settled_wait_ignores_a_screen_replaced_before_output_settles` pins this behaviour: with the old single check it failed 5 of 5 runs, returning a `busy` screen. Snapshot and profile sessions on the layout fixture wait for exactly `192238 entries ` (`wait_layout_fixture_ready`). During a six-digit scan the cell after `entries` keeps its `…` and is never rewritten, so only the Ready frame satisfies that predicate. With the fix, 60 loaded runs had no snapshot failures.

**Open.** A snapshot can still freeze a torn frame after any other wait, as the help-screen failures show. The complete fix needs a frame boundary the harness can observe: fdu emitting synchronized output (`CSI ? 2026 h`/`l`) around each frame, and ptytest holding the screen until the update ends.
||||||| parent of 8e8ebf85f (fix(verify): report raw-byte glob fixture as not run on APFS)

## DSLP-30

**Cause (established).** Expansion case `expansion-00011` (`echo bridge-*.txt`) creates a fixture file named `bridge-\xff.txt` in `.work/tmp/semantic-bridge-expansion` (`examples/semantic_bridge/expansion.rs`, `with_glob_directory`). APFS and HFS+ reject non-UTF-8 names with `EILSEQ`, so generation aborted the whole bridge run. Reproduced at base `a9b1966fd` on Darwin arm64 with `export TMPDIR="$PWD/.work/tmp" PATH="$HOME/.elan/bin:$PATH"; make verify` (exit 2): `semantic bridge: cannot create expansion match fixture: Illegal byte sequence (os error 92)`. `cargo test -p ish --example semantic_bridge` failed the same way (exit 101, `output_and_glob_fixtures_preserve_raw_bytes_and_unicode_scalars`).

**No byte-preserving filesystem on this host.** Probed on a `hdiutil attach -nomount ram://65536` disk, creating `bridge-\xff.txt` through Python: HFS+ stored `bridge-%FF.txt` (name changed), MS-DOS stored `bridge-%FF.txt` plus an `._` file, ExFAT failed with `EINVAL`, UDF cannot be formatted, and `mount_tmpfs` needs root (`Operation not permitted`). Case 11 therefore cannot run on macOS without a Linux filesystem (container or VM).

**Fix.** `EILSEQ` on a non-UTF-8 name now raises a `raw_filenames_unsupported` error. The runner records the case as not run and keeps generating; it prints `<family>: NOT RUN <case>: raw-byte file name case; <reason>` after the family line, and the case is excluded from the Lean request (so the family reports 999 generated, not 1000). Every other case runs with unchanged assertions, and any other fixture error still fails. A case selected with `--id` that cannot run still fails (exit 1). The glob unit test is split from the output unit test; it prints `NOT RUN expansion-00011 ...` (visible with `--nocapture`) and returns early only on that error. On Linux the file is created and nothing changes.

```bash
export TMPDIR="$PWD/.work/tmp" PATH="$HOME/.elan/bin:$PATH"
cargo test -p ish --example semantic_bridge -- --nocapture
cargo run --quiet -p ish --example semantic_bridge -- --family expansion --cases 1000
```

Before: exit 101 and exit 1 with the `Illegal byte sequence` error. After: exit 0 (17 passed) and exit 0 (`expansion: 999 generated, 0 retained, comparator control rejected`, then the `NOT RUN expansion-00011` line).

**Not resolved by this change.** `make verify` still exits 2 on this host, now in the effects family, identically at base: `effects worker exited with exit status: 1: semantic bridge: cannot inspect descriptor 5: Bad file descriptor (os error 9)` (`examples/semantic_bridge/effects.rs`, `descriptor_resource` accepts only `NotFound` from `/dev/fd/N`). `make verify-mutations` fails on its effects baseline for the same reason. Treating `EBADF` as closed in a temporary patch got past that error but produced a real mismatch at `effects-00010` (expected no descriptors, Lean expected `[[3,7],[4,8]]`), so it is a separate defect and was not changed.
||||||| parent of bb8f8f757 (docs(deslop): record the DSLP-36 effects descriptor cause and fix)

## DSLP-36

Fixed. Base `5cc47767d`, Darwin arm64. `make verify` stops in the bridge's effects family. The expansion family fails first on APFS raw filenames (fixed in PR #42), so the family was run directly after `cd formal && lake build semanticBridge`:

```bash
export TMPDIR="$PWD/.work/tmp" PATH="$HOME/.elan/bin:$PATH"
cargo run --quiet -p ish --example semantic_bridge -- --family effects --cases 1000
```

At base this exited 1 with `semantic bridge: effects worker exited with exit status: 1: semantic bridge: cannot inspect descriptor 5: Bad file descriptor (os error 9)`. The ticket's `exit 2` is the `make verify` status.

**Cause (established).** The worker is one process: epsh runs `exec N>file` in it, then `descriptor_resource` inspects the same process, so the inspection is in the right process and descriptors are still open. The defect is the probe. It compared `stat("/dev/fd/N")` with the `(st_dev, st_ino)` of the resource file. On Linux `/dev/fd` is a symlink to `/proc/self/fd`, so the stat follows to the file. On Darwin `/dev/fd/N` is a devfs node whose stat reports devfs's device number and a rewritten mode, and only the inode matches. Checked with `python3 -I`: for an open file, `os.fstat(3)` gave `(16777232, 394858068, 0o100644)` and `os.stat("/dev/fd/3")` gave `(1139203528, 394858068, 0o100200)`. A closed descriptor gives `EBADF`, not `ENOENT`. So the base code failed on the first closed probe, and accepting `EBADF` alone would have reported every open descriptor as missing, matching no resource. That is the "Rust sees no descriptors, Lean expects `[[3,7],[4,8]]`" mismatch at effects-00010. Lean and epsh were correct.

**Fix.** `descriptor_resource` (`examples/semantic_bridge/effects.rs`) calls `fstat` on the descriptor itself, through a `ManuallyDrop<File>` so the shell's descriptor is never closed, and treats `EBADF` as closed. This is the same on both OSes, with no `/dev/fd` or `/proc` dependency. The Lean model and product code are unchanged.

**Proof.**

| Command | Base | Fixed |
| --- | --- | --- |
| `cargo run --quiet -p ish --example semantic_bridge -- --family effects --cases 1000` | exit 1, `cannot inspect descriptor 5` | exit 0, `effects: 1000 generated, 0 retained, comparator control rejected` |
| `python3 -B scripts/test-semantic-mutations.py` | not run | exit 0, 2 tests OK |
| `python3 scripts/check-semantic-mutations.py` | effects baseline failed the same way | exit 1 on this branch alone, because the expansion baseline dies on APFS raw filenames before effects runs; exit 0 with PR #42's `expansion.rs`, `mod.rs` and `runner.rs` applied temporarily (`baseline effects: passed`, `killed saved descriptor restoration: ... effects-00010`, `mutation run: 4 compiled mutants killed by semantic assertions`), then restored |
| `cargo clippy --quiet -p ish --example semantic_bridge` | not run | exit 0, no output |

## DSLP-35

Fixed (harness). The ticket does not name the test or wait. Eight full-suite runs under sixteen burners plus process churn produced `timed out after 3000ms waiting for "$ "; output: ""` three times, in `native_fs_static_dynamic_and_nested_fork_contexts_refuse_before_redirections`, `denv_rejects_parent_helpers_after_parent_revocation_or_edit`, and `fatal_crash_signals_restore_the_terminal_and_exit_with_the_signal_status`. This is `PtyShell::spawn_launched`, which every shell-spawning test runs, including the OSC 133 ones. No OSC 133 byte was ever written: the shell had printed nothing in 3 s.

**Cause (scheduling latency against a test-chosen deadline).** The wait was 3 s. Startup latency to the first prompt, measured with a script that forks ish on a pty (as the harness does, with the same environment) and times the first byte and the `$ ` under sixteen `yes` burners and the `/usr/bin/true` loop on this 18-core host (other lanes also loaded it):

```
par=1   n=5    p50 0.087 s  max 0.119 s
par=18  n=360  p50 0.093 s  p90 0.556 s  p99 0.976 s  max 1.160 s
par=40  n=400  p50 0.457 s  p90 1.710 s  p99 3.315 s  max 4.761 s  (5 over 3 s)
```

The first byte and the prompt coincide, so startup is not blocked in ish; the process waits to be scheduled. Eighteen concurrent test threads plus other load reach the tail seen in the failures.

**Fix.** `STARTUP_PROMPT_TIMEOUT_MS` is 30 s, six times the measured maximum. The wait returns at the first prompt, so passing runs are not slower. `startup_prompt_wait_covers_scheduling_latency_before_ish_runs` starts ish behind a 4 s sleep; it failed with the ticket's message at 3 s and passes at 30 s. Not re-measured under load after the change: a natural failure rate of about 3 in 8 suite runs makes eight runs too few to resolve.

**Other suite failures under load.** The same sixteen suite runs failed other tests on waits of 3 to 5 s for an external command or a first prompt (`pty-child-ready`, `input-ready`, `/bin/sh -c 'exit 0' 0<&-`, `./bin/exec`, `selected]`); they share the cause above and were not changed. `native_fs_ctrl_z_is_ignored_during_copy_and_external_job_control_still_works` timed out after 15 s in all 16 suite runs and in an isolated run on this host, so it is not load-dependent here. `native_fs_quick_no_progress_copy_stays_quiet` asserts that a quick copy prints no progress, which a slow host breaks. Neither is fixed.
||||||| parent of ceca05042 (docs(flakes): record the DSLP-31 doctest E0463 investigation)

## DSLP-31

Darwin arm64, rustc and cargo nightly-2026-11-09, kache 2.0.0-rc.2 via `~/.cargo/config.toml` (`build.rustc-wrapper = "kache"`), host load average about 49. Every command used `TMPDIR="$PWD/.work/tmp" CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$PWD/.work/tmp/target"`.

**Not reproduced by running the suite.** Two cold targets (`rm -rf` of the target dir, then `cargo test -p fdu-delete --doc`, with kache hits for the registry crates) passed, and `cargo test --doc` over the default members passed on the warm target. `make test` on a cold target was killed by SIGTERM and SIGKILL mid-compile (cause not established), and a retry stopped at an unrelated failure, `conformance::executor::tests::timeout_kills_the_process_group_and_reaps_the_child` (`crates/epsh/tests/conformance/executor.rs:599`, `unwrap()` on `None`), before any doctest ran. Cargo stops at the first failing test binary, so doctests run only after every test passes, as the ticket describes.

**Reproduced by mutating the target during the run.** The failure text and position match the ticket exactly. A `RUSTDOC` wrapper delays the doctest phase, so a second process can act after Cargo's build:

```bash
cat > .work/tmp/bin/slow-rustdoc <<'SH'
#!/bin/sh
sleep "${SLOW:-30}"
exec "$(rustup which rustdoc)" "$@"
SH
chmod +x .work/tmp/bin/slow-rustdoc
cargo test -p fdu-delete --doc >/dev/null   # warm the target
(RUSTDOC=$PWD/.work/tmp/bin/slow-rustdoc SLOW=30 cargo test -p fdu-delete --doc > .work/tmp/out.log 2>&1) &
sleep 15; cargo clean -p crossbeam-utils; wait; rg -A3 E0463 .work/tmp/out.log
```

Result: `error[E0463]: can't find crate for 'crossbeam_utils' which 'rayon' depends on --> crates/fdu-delete/src/lib.rs:563:26`, `doctest failed`, exit 1. A second run, with `cargo build -p fdu-delete` instead of the clean, found nothing to do.

**Cause (established for the failure; the trigger in the incident is not).**

- Cargo builds, drops its build lock, runs the test binaries, and only then runs rustdoc for each library. `cargo build -p fdu-delete` run 8 s into a delayed doctest returned in 0.18 s and was not blocked, so concurrent Cargo commands share the target while a test run is still using it.
- Rustdoc runs even for a crate with no doctests. `crates/fdu-delete/src/lib.rs` has none (`running 0 tests`), but the invocation still loads `rayon`'s metadata, and rustc then needs `crossbeam_utils`, `crossbeam_epoch`, `crossbeam_deque` and `rayon_core` from the `-L dependency=` directories (`target/debug/build/<crate>/<hash>/out/`). Only `fdu-delete` among the default members depends on `rayon`, so it is the only doctest that reads those files.
- Cargo deletes a unit's previous outputs before it starts the compiler. With `RUSTC_WRAPPER` set to a script that lists the rlib before `exec kache`, the rlib was already gone (`No such file or directory`), so the artifact is absent for the whole compile or kache restore, not just a swap. This is Cargo behavior, not kache's.
- With a loop rerunning the exact rustdoc command from `cargo test -p fdu-delete --doc -v` (`.work/tmp/rustdoc-loop.sh`), ten forced rebuilds of the `crossbeam-utils` chain (`rm -rf` of its `fingerprint` directory, then `cargo build -p fdu-delete`, each about 50 s under this load) produced 1634 failures in 2944 runs, all E0463 naming `crossbeam_utils`, `crossbeam_epoch`, `crossbeam_deque` or `rayon_core`. The same loop with no rebuild passed 45 of 45.

So E0463 appears whenever another process removes or rebuilds a transitive dependency's artifacts between Cargo's freshness check and the doctest phase. `cargo clean -p`, `cargo clean`, `rm -rf target`, or any Cargo run that finds those units dirty qualifies. Kache's part is latency: a hit still goes through the wrapper while the artifact is absent, and `kache stats`, `kache doctor` and `kache daemon` all report the daemon as not reachable and not starting (`WARN daemon did not start after recovery`; LaunchAgent `ninja.kunobi.kache` installed). Each wrapper call then runs without the daemon, which lengthens the window.

**What was ruled out as a trigger.** Alternating `make test`'s environment (`CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=clang MUSL_TARGET MUSL_BUILD_MODE`) with a plain `cargo build -p fdu-delete -v`, and alternating `-p fdu-delete`, default members and `--workspace` for `cargo test --no-run -v`, left the registry units fresh after the first round. The linker setting dirties only workspace units ("config settings changed"). Nothing in the Makefile, scripts or tests removes or rebuilds a registry dependency: the only `cargo clean` is `make release`, which cleans `-p ish --profile dist --target`, and the tests delete only uniquely named scratch directories. Which process mutated the target in the reported run was not recorded, so it stays unidentified.

**Disposition.** No product, Makefile or test change. `doctest = false` on crates with no doctests would hide the symptom for `fdu-delete`, and would silently skip any doctest added later, so it was not adopted; it also would not stop the same mutation from breaking `epsh`'s four doctests. Do not run `cargo clean`, `rm -rf` of the target, or any Cargo command that can rebuild registry units in a worktree while its `make test` is running; use a separate `CARGO_TARGET_DIR` for the second command. If it recurs, run `rg 'E0463' ` on the log, then `ls -l target/debug/build/<named crate>/*/out/` right away to see whether the rlib is absent (a concurrent rebuild, which a re-run clears) or present (a different defect).
