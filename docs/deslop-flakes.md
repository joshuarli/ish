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
