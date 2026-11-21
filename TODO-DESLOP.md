Prefix: DSLP
Next: 36

Deslop campaign backlog ([plan](https://github.com/laputa-systems/stuff/blob/main/docs/campaigns/deslop.md), tier D). One agent works this file with the `deslop` skill: take the lowest-numbered open ticket whose area does not start with `Needs owner ·`. Complete the component tickets before the findings and final report.

## Report

Ground base: `fc62468e2428f9aea6fcd611b0820ab709ce0c65`. The first cold `make test` took 137.96s and failed because the callback test passed a multi-digit descriptor to `/bin/sh` redirection. The test now reads that descriptor via `/dev/fd`. The next run took 129.63s and hit two terminal-decoder timeouts; both passed in isolation. A base worktree library run reproduced one timeout (DSLP-27). Timings are busy-host observations, not performance gates.

A later full test run reported no failed tests in 169.24s, but its wrapper failed to capture the exit code. Another run exited 2 in 121.50s when the large-output PTY test started its prompt deadline on the echoed command. Its marker now waits for the final output line; the focused test passed, and a full rebuild and test exited 0 in 479.00s. A previous retry also timed out in two other PTY tests; their causes and base equivalence were not established.

Needs environment: `gha-test-workflow` is unavailable (exit 127); both pull-request workflows already invoke the required make targets. A quiet performance host was unavailable (`perf-gate-ready` exit 75).

## Tickets

### DSLP-27 · P2 · Terminal decoder framing tests time out during the full library suite

- **Area:** Needs environment · terminal input tests
- **Where:** src/input.rs:1061, src/input.rs:1107, src/input.rs:1112
- **Problem:** On a busy macOS host, make test timed out in replies_between_typed_keys_leave_the_typing_intact and reply_and_report_sequences_are_consumed_whole_at_every_split. Both passed isolated. A full ish library run at base fc62468e repeated the first timeout; it did not repeat the second. The root cause is not established, and intermittent suite failures obscure regressions.
- **Recommendation:** Reproduce under controlled load, determine whether framing, the test harness, or scheduling causes the sentinel timeout, and fix the cause without weakening the behavior assertion.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-28 · P2 · PTY completion tests intermittently time out under load

- **Area:** Needs environment · PTY tests
- **Where:** tests/pty.rs:453, tests/pty.rs:506
- **Problem:** A busy macOS full-suite retry timed out in builtin_lookup_completion_and_explicit_paths_share_the_policy and completion_resize_rerenders_grid. A later full suite passed; neither failure was reproduced at base and the cause remains unknown, so they can obscure regressions.
- **Recommendation:** Reproduce each failure under controlled load and at the base revision; identify whether the shell or harness is responsible before changing deadlines or assertions.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-30 · P2 · Semantic bridge raw filename fixtures fail on macOS filesystems

- **Area:** Needs environment · semantic verification
- **Where:** examples/semantic_bridge/expansion.rs:1114, examples/semantic_bridge/expansion.rs:1139, formal/README.md
- **Problem:** On macOS APFS, make verify fails with Illegal byte sequence when the expansion bridge creates a fixture containing invalid UTF-8 filename bytes. This prevents the documented full local verification on that filesystem; the byte-preserving fixtures run on a Linux filesystem.
- **Recommendation:** Provide and document a byte-preserving filesystem for these fixtures on supported hosts without reducing raw-byte coverage.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-31 · P2 · Full macOS test run can fail to locate a transitive crate in doctests

- **Area:** Needs environment · Rust doctests
- **Where:** crates/fdu-delete/src/lib.rs:563, Makefile:20
- **Problem:** On a macOS make test run using the configured kache wrapper, the tests passed but the fdu-delete doctest failed with E0463: cannot find crossbeam_utils which rayon depends on. The cause is unproven; the failure prevents full-suite verification.
- **Recommendation:** Reproduce the rustdoc artifact lookup failure and fix the underlying toolchain or cache interaction without suppressing doctests.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-32 · P2 · Process picker PTY test can lose its snapshot under load

- **Area:** Needs environment · PTY tests
- **Where:** tests/pty.rs:453, tests/pty.rs
- **Problem:** On a busy macOS make test run, process_picker_cancel_no_match_and_explicit_term_kill_acceptance timed out waiting for selected after pk reported process lookup failed: process changed during snapshot. The cause is unproven; this obscures full-suite results.
- **Recommendation:** Reproduce under controlled process churn and determine why the fixture snapshot changes before changing the test or picker behavior.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-34 · P2 · fdu PTY snapshots can capture a partially written frame

- **Area:** Needs environment · PTY tests
- **Where:** crates/fdu-tui/src/lib.rs:210, tests/fdu_support.rs, laputa-ptytest screen model
- **Problem:** fdu writes each frame as a cell diff with no boundary marker, and ptytest's screen shows whatever bytes have arrived. Under load a torn frame can stay quiet for longer than the 25 ms settle window. The ready wait is now tear-proof (docs/deslop-flakes.md, DSLP-34), but any other wait can still settle on a partial frame: help_screen_opens_and_closes failed 3/60 loaded runs that way.
- **Recommendation:** Have fdu bracket each frame in synchronized output (CSI ? 2026 h / l), and have laputa-ptytest withhold screen updates from predicates and snapshots until the update ends. Then rerun the loaded soak in docs/deslop-flakes.md.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-35 · P2 · PTY suite can time out waiting for the OSC 133 prompt marker under load

- **Area:** Needs environment · PTY tests
- **Where:** tests/pty.rs
- **Problem:** A loaded Darwin make test at fix/deslop-flakes (docs/deslop-flakes.md research run, 2026-11-20) timed out waiting for an OSC 133 prompt marker after 216 PTY passes; the rerun failed instead with a process picker Invalid argument (os error 22). Neither failure is tied to a code change and the cause is unproven.
- **Recommendation:** Record which test and wait timed out, reproduce it alone under the recorded load, and fix the harness or shell cause without lengthening the wait unless the cause is proven to be scheduling latency against a deadline the test chose.
- **Fix with:** root-cause
- **Effort:** M

