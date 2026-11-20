Prefix: DSLP
Next: 33

Deslop campaign backlog ([plan](https://github.com/laputa-systems/stuff/blob/main/docs/campaigns/deslop.md), tier D). One agent works this file with the `deslop` skill: take the lowest-numbered open ticket whose area does not start with `Needs owner ·`. Complete the component tickets before the findings and final report.

## Report

Ground base: `fc62468e2428f9aea6fcd611b0820ab709ce0c65`. The first cold `make test` took 137.96s and failed because the callback test passed a multi-digit descriptor to `/bin/sh` redirection. The test now reads that descriptor via `/dev/fd`. The next run took 129.63s and hit two terminal-decoder timeouts; both passed in isolation. A base worktree library run reproduced one timeout (DSLP-27). Timings are busy-host observations, not performance gates.

A later full test run reported no failed tests in 169.24s, but its wrapper failed to capture the exit code. Another run exited 2 in 121.50s when the large-output PTY test started its prompt deadline on the echoed command. Its marker now waits for the final output line; the focused test passed, and a full rebuild and test exited 0 in 479.00s. A previous retry also timed out in two other PTY tests; their causes and base equivalence were not established.

Needs environment: `gha-test-workflow` is unavailable (exit 127); both pull-request workflows already invoke the required make targets. A quiet performance host was unavailable (`perf-gate-ready` exit 75).

## Tickets

### DSLP-3 · P2 · Types: shell execution and persistence

- **Area:** deslop · types
- **Where:** src/shell.rs, src/dispatch.rs, src/foreground.rs, src/signal.rs, src/history.rs, src/history/, src/config.rs, src/builtin.rs
- **Problem:** Strict-profile candidates and typing findings remain unmeasured for shell execution and persistence. Contracts and consumers: interactive dispatch, job control, signals, config and history journal; consumed by the ish binary and PTY tests.
- **Recommendation:** Measure applicable candidates; adopt only witnessed settings, keep escape hatches from increasing and record witnesses. Acceptance: cargo test -p ish --lib --tests; meet the deslop types acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-4 · P2 · Tests: shell execution and persistence

- **Area:** deslop · tests
- **Where:** src/shell.rs, src/dispatch.rs, src/foreground.rs, src/signal.rs, src/history.rs, src/history/, src/config.rs, src/builtin.rs
- **Problem:** Boundary tests and bounded mutation remain unmeasured for shell execution and persistence. Contracts and consumers: interactive dispatch, job control, signals, config and history journal; consumed by the ish binary and PTY tests.
- **Recommendation:** Inventory public contracts and platform gaps, exercise critical decisions with bounded mutation, disposition survivors and preserve the test cost budget. Acceptance: cargo test -p ish --lib --tests; meet the deslop tests acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-5 · P2 · Dead code: shell execution and persistence

- **Area:** deslop · dead code
- **Where:** src/shell.rs, src/dispatch.rs, src/foreground.rs, src/signal.rs, src/history.rs, src/history/, src/config.rs, src/builtin.rs
- **Problem:** Dead-code tools and debloat remain unmeasured for shell execution and persistence. Contracts and consumers: interactive dispatch, job control, signals, config and history journal; consumed by the ish binary and PTY tests.
- **Recommendation:** Run applicable dead-code tools, check consumers and platforms, allowlist only justified findings and file walk-backs for owner decision. Acceptance: cargo test -p ish --lib --tests; meet the deslop dead code acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-6 · P2 · Types: editor and shell utilities

- **Area:** deslop · types
- **Where:** src/ excluding the shell execution and persistence files
- **Problem:** Strict-profile candidates and typing findings remain unmeasured for editor and shell utilities. Contracts and consumers: terminal input, completion, rendering and shell-owned utilities; consumed by the ish binary and PTY tests.
- **Recommendation:** Measure applicable candidates; adopt only witnessed settings, keep escape hatches from increasing and record witnesses. Acceptance: cargo test -p ish --lib --tests; meet the deslop types acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-7 · P2 · Types: epsh engine

- **Area:** deslop · types
- **Where:** crates/epsh/src/
- **Problem:** Strict-profile candidates and typing findings remain unmeasured for epsh engine. Contracts and consumers: parser, expansion, evaluation and embedding API; consumed by ish and epsh integration tests.
- **Recommendation:** Measure applicable candidates; adopt only witnessed settings, keep escape hatches from increasing and record witnesses. Acceptance: make test-epsh; meet the deslop types acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-8 · P2 · Types: epsh fixtures and integration

- **Area:** deslop · types
- **Where:** crates/epsh/tests/, crates/epsh/check-epsh.t
- **Problem:** Strict-profile candidates and typing findings remain unmeasured for epsh fixtures and integration. Contracts and consumers: byte-preserving conformance, embedding and evaluator contracts; consumed by epsh and ish.
- **Recommendation:** Measure applicable candidates; adopt only witnessed settings, keep escape hatches from increasing and record witnesses. Acceptance: make test-epsh; meet the deslop types acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-9 · P2 · Types: filesystem engine

- **Area:** deslop · types
- **Where:** crates/ish-fs/
- **Problem:** Strict-profile candidates and typing findings remain unmeasured for filesystem engine. Contracts and consumers: filesystem operation and library API contracts; consumed by ish, fdu and native filesystem tests.
- **Recommendation:** Measure applicable candidates; adopt only witnessed settings, keep escape hatches from increasing and record witnesses. Acceptance: cargo test -p ish-fs; meet the deslop types acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-10 · P2 · Types: fdu components

- **Area:** deslop · types
- **Where:** crates/fdu-*/
- **Problem:** The fdu contracts and library boundaries need the deslop pass. Consumers: ish and fdu integration tests.
- **Recommendation:** Complete the deslop types acceptance for these crates; run make test as the consumer acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-11 · P2 · Tests: editor and shell utilities

- **Area:** deslop · tests
- **Where:** src/ excluding the shell execution and persistence files
- **Problem:** Boundary tests and bounded mutation remain unmeasured for editor and shell utilities. Contracts and consumers: terminal input, completion, rendering and shell-owned utilities; consumed by the ish binary and PTY tests.
- **Recommendation:** Inventory public contracts and platform gaps, exercise critical decisions with bounded mutation, disposition survivors and preserve the test cost budget. Acceptance: cargo test -p ish --lib --tests; meet the deslop tests acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-12 · P2 · Tests: epsh engine

- **Area:** deslop · tests
- **Where:** crates/epsh/src/
- **Problem:** Boundary tests and bounded mutation remain unmeasured for epsh engine. Contracts and consumers: parser, expansion, evaluation and embedding API; consumed by ish and epsh integration tests.
- **Recommendation:** Inventory public contracts and platform gaps, exercise critical decisions with bounded mutation, disposition survivors and preserve the test cost budget. Acceptance: make test-epsh; meet the deslop tests acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-13 · P2 · Tests: epsh fixtures and integration

- **Area:** deslop · tests
- **Where:** crates/epsh/tests/, crates/epsh/check-epsh.t
- **Problem:** Boundary tests and bounded mutation remain unmeasured for epsh fixtures and integration. Contracts and consumers: byte-preserving conformance, embedding and evaluator contracts; consumed by epsh and ish.
- **Recommendation:** Inventory public contracts and platform gaps, exercise critical decisions with bounded mutation, disposition survivors and preserve the test cost budget. Acceptance: make test-epsh; meet the deslop tests acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-14 · P2 · Tests: filesystem engine

- **Area:** deslop · tests
- **Where:** crates/ish-fs/
- **Problem:** Boundary tests and bounded mutation remain unmeasured for filesystem engine. Contracts and consumers: filesystem operation and library API contracts; consumed by ish, fdu and native filesystem tests.
- **Recommendation:** Inventory public contracts and platform gaps, exercise critical decisions with bounded mutation, disposition survivors and preserve the test cost budget. Acceptance: cargo test -p ish-fs; meet the deslop tests acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-15 · P2 · Tests: fdu components

- **Area:** deslop · tests
- **Where:** crates/fdu-*/
- **Problem:** The fdu contracts and library boundaries need the deslop pass. Consumers: ish and fdu integration tests.
- **Recommendation:** Complete the deslop tests acceptance for these crates; run make test as the consumer acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-16 · P2 · Dead code: editor and shell utilities

- **Area:** deslop · dead code
- **Where:** src/ excluding the shell execution and persistence files
- **Problem:** Dead-code tools and debloat remain unmeasured for editor and shell utilities. Contracts and consumers: terminal input, completion, rendering and shell-owned utilities; consumed by the ish binary and PTY tests.
- **Recommendation:** Run applicable dead-code tools, check consumers and platforms, allowlist only justified findings and file walk-backs for owner decision. Acceptance: cargo test -p ish --lib --tests; meet the deslop dead code acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-17 · P2 · Dead code: epsh engine

- **Area:** deslop · dead code
- **Where:** crates/epsh/src/
- **Problem:** Dead-code tools and debloat remain unmeasured for epsh engine. Contracts and consumers: parser, expansion, evaluation and embedding API; consumed by ish and epsh integration tests.
- **Recommendation:** Run applicable dead-code tools, check consumers and platforms, allowlist only justified findings and file walk-backs for owner decision. Acceptance: make test-epsh; meet the deslop dead code acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-18 · P2 · Dead code: epsh fixtures and integration

- **Area:** deslop · dead code
- **Where:** crates/epsh/tests/, crates/epsh/check-epsh.t
- **Problem:** Dead-code tools and debloat remain unmeasured for epsh fixtures and integration. Contracts and consumers: byte-preserving conformance, embedding and evaluator contracts; consumed by epsh and ish.
- **Recommendation:** Run applicable dead-code tools, check consumers and platforms, allowlist only justified findings and file walk-backs for owner decision. Acceptance: make test-epsh; meet the deslop dead code acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-19 · P2 · Dead code: filesystem engine

- **Area:** deslop · dead code
- **Where:** crates/ish-fs/
- **Problem:** Dead-code tools and debloat remain unmeasured for filesystem engine. Contracts and consumers: filesystem operation and library API contracts; consumed by ish, fdu and native filesystem tests.
- **Recommendation:** Run applicable dead-code tools, check consumers and platforms, allowlist only justified findings and file walk-backs for owner decision. Acceptance: cargo test -p ish-fs; meet the deslop dead code acceptance for this component.
- **Fix with:** deslop
- **Effort:** M

### DSLP-20 · P2 · Dead code: fdu components

- **Area:** deslop · dead code
- **Where:** crates/fdu-*/
- **Problem:** The fdu contracts and library boundaries need the deslop pass. Consumers: ish and fdu integration tests.
- **Recommendation:** Complete the deslop dead code acceptance for these crates; run make test as the consumer acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-21 · P2 · Types: shell integration tests

- **Area:** deslop · types
- **Where:** tests/
- **Problem:** The shell CLI, PTY and persistence integration contracts need the deslop pass. Consumers: ish users and maintainers.
- **Recommendation:** Complete the deslop types acceptance for these tests; run make test as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-22 · P2 · Tests: shell integration tests

- **Area:** deslop · tests
- **Where:** tests/
- **Problem:** The shell CLI, PTY and persistence integration contracts need the deslop pass. Consumers: ish users and maintainers.
- **Recommendation:** Complete the deslop tests acceptance for these tests; run make test as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-23 · P2 · Dead code: shell integration tests

- **Area:** deslop · dead code
- **Where:** tests/
- **Problem:** The shell CLI, PTY and persistence integration contracts need the deslop pass. Consumers: ish users and maintainers.
- **Recommendation:** Complete the deslop dead code acceptance for these tests; run make test as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-24 · P2 · Types: examples and workloads

- **Area:** deslop · types
- **Where:** examples/, benches/, scripts/
- **Problem:** Example and benchmark entrypoints need the deslop pass. Consumers: ish maintainers and library users.
- **Recommendation:** Complete the deslop types acceptance for these files; run cargo check --workspace --all-targets as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-25 · P2 · Tests: examples and workloads

- **Area:** deslop · tests
- **Where:** examples/, benches/, scripts/
- **Problem:** Example and benchmark entrypoints need the deslop pass. Consumers: ish maintainers and library users.
- **Recommendation:** Complete the deslop tests acceptance for these files; run cargo check --workspace --all-targets as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

### DSLP-26 · P2 · Dead code: examples and workloads

- **Area:** deslop · dead code
- **Where:** examples/, benches/, scripts/
- **Problem:** Example and benchmark entrypoints need the deslop pass. Consumers: ish maintainers and library users.
- **Recommendation:** Complete the deslop dead code acceptance for these files; run cargo check --workspace --all-targets as the acceptance command.
- **Fix with:** deslop
- **Effort:** M

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
