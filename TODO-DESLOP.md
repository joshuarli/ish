Prefix: DSLP
Next: 43

Deslop campaign backlog ([plan](https://github.com/laputa-systems/stuff/blob/main/docs/campaigns/deslop.md), tier D). One agent works this file with the `deslop` skill: take the lowest-numbered open ticket whose area does not start with `Needs owner ·`. Complete the component tickets before the findings and final report.

## Report

Ground base: `fc62468e2428f9aea6fcd611b0820ab709ce0c65`. The first cold `make test` took 137.96s and failed because the callback test passed a multi-digit descriptor to `/bin/sh` redirection. The test now reads that descriptor via `/dev/fd`. The next run took 129.63s and hit two terminal-decoder timeouts; both passed in isolation. A base worktree library run reproduced one timeout (DSLP-27). Timings are busy-host observations, not performance gates.

A later full test run reported no failed tests in 169.24s, but its wrapper failed to capture the exit code. Another run exited 2 in 121.50s when the large-output PTY test started its prompt deadline on the echoed command. Its marker now waits for the final output line; the focused test passed, and a full rebuild and test exited 0 in 479.00s. A previous retry also timed out in two other PTY tests; their causes and base equivalence were not established.

Needs environment: `gha-test-workflow` is unavailable (exit 127); both pull-request workflows already invoke the required make targets. A quiet performance host was unavailable (`perf-gate-ready` exit 75).

## Tickets

### DSLP-34 · P2 · fdu PTY snapshots can capture a partially written frame

- **Area:** Needs environment · PTY tests
- **Where:** crates/fdu-tui/src/lib.rs:210, tests/fdu_support.rs, laputa-ptytest screen model
- **Problem:** fdu writes each frame as a cell diff with no boundary marker, and ptytest's screen shows whatever bytes have arrived. Under load a torn frame can stay quiet for longer than the 25 ms settle window. The ready wait is now tear-proof (docs/deslop-flakes.md, DSLP-34), but any other wait can still settle on a partial frame: help_screen_opens_and_closes failed 3/60 loaded runs that way.
- **Recommendation:** Have fdu bracket each frame in synchronized output (CSI ? 2026 h / l), and have laputa-ptytest withhold screen updates from predicates and snapshots until the update ends. Then rerun the loaded soak in docs/deslop-flakes.md.
- **Fix with:** root-cause
- **Effort:** M

### DSLP-39 · P3 · Quick native copy test asserts no progress output, which a slow host breaks

- **Area:** PTY tests
- **Where:** tests/pty.rs (native_fs_quick_no_progress_copy_stays_quiet, ~line 1021)
- **Problem:** The test asserts a quick copy prints no progress, but the progress threshold is wall-clock, so a slow or loaded host shows progress and fails it.
- **Recommendation:** Drive the quiet decision with a controllable threshold or a copy small enough to be bounded by work, not time.
- **Fix with:** root-cause
- **Effort:** S
