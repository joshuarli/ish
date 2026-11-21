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

Unreproduced and unresolved. Each of the two decoder tests passed 3/3 runs under eight burners and 10/10 under sixteen burners, with the process-churn loop in both runs. In each log Cargo reported one test run. `src/input.rs:1061` writes a `Z` sentinel and `src/input.rs:1105` waits 20 seconds for the reader; `src/input.rs:1119` runs split cases concurrently. These are observations, not proof whether a lost sentinel, framing error, or scheduling caused the reported timeout. No fix or post-fix loop was attempted without a failing reproduction.

## DSLP-28

Unreproduced and unresolved. Both completion tests passed 3/3 runs at the base commit under eight burners and 10/10 runs against the unchanged base source under sixteen burners, with process churn in both runs. The first documentation-only commit was made during the second loop; no test or product source changed. Each log reported one test run. `tests/pty.rs:427` waits for output markers, while `tests/pty.rs:1671` and `tests/pty.rs:2778` wait for quiescence before inspecting completion state. The passing runs do not establish which wait or shell state caused either reported timeout. No fix or post-fix loop was attempted without a failing reproduction.

## DSLP-32

Unresolved. The picker test passed 3/3 runs at the base commit under eight burners and failed 1/10 runs against unchanged base source under sixteen burners, with process churn in both runs. The failure was `pk: process lookup failed: Input/output error (os error 5)` before `selected]`, not the ticket's `process changed during snapshot`. `src/pk.rs:117` scans processes and `src/pk.rs:134` binds each eligible record; `src/pk/macos.rs:137` emits the ticket's distinct error if a record changes between reads. The captured error does not identify the process or prove that it was the fixture. No fix or post-fix loop was attempted without reproducing the ticket's error and establishing its cause.

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
