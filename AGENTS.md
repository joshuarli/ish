ish is an interactive-only shell written in Rust. The README is the user-facing
behavior contract.

## Commands

- `make build` builds the default workspace members.
- `make test` tests the default members, including PTY and conformance tests.
- `make test-epsh` runs epsh library and integration tests.
- `make conformance` prints all retained native conformance case results.
- `make verify` checks Lean models, theorem axioms, the Rust bridge, and production mutations.
- `make verify-refinement` checks the extracted Rust shift-count proof on Linux.
- `make check` runs workspace lint, Rust tests, and semantic verification.
- `cargo check --workspace` checks the Rust workspace.

## Verification

- `make test`: default members, PTY and native conformance tests; busy macOS runs failed after 138s cold and 130s warm (see `TODO-DESLOP.md`).
- `make lint`: workspace Clippy and Ruff for scripts; 116s on a busy macOS host with three Cargo jobs.
- `make verify`: Lean model, differential bridge, and mutation checks; cost not measured here.
- CLI flags and argument errors: `tests/cli_help.rs`, `tests/login_shell.rs`, `src/main.rs` tests.
- Exit codes and dispatch policy: `tests/login_shell.rs`, `tests/pty.rs`.
- Config format: `src/config.rs` tests and `tests/integration.rs`.
- History journal and legacy import: `src/history/persistence_tests.rs`, `tests/history_import.rs`, `tests/pty.rs`.
- epsh embedding API: `crates/epsh/tests/embedding.rs` and `api_stability.rs`; consumer: ish.
- ish-fs operations and API: `crates/ish-fs/tests/`; consumers: ish and fdu.
- Performance: `make bench` uses `benches/bench.rs` and `benches/baseline.json`; filesystem: `benches/fs/README.md`.
- Components: shell runtime and history, editor and utilities, epsh engine and fixtures, ish-fs, fdu, integration and workloads.
- Vendored fz: https://github.com/laputa-systems/fz at `8a91d565dff85caac4131a4bde90f5c685812590` (`vendor/fz/UPSTREAM`).

## Invariants

- Keep ish interactive-only; adding scripts, POSIX compatibility, `source`, `eval`, control flow, functions, background jobs, plugins, themes, or prompt configuration requires an explicit design decision.
- Keep shell-owned operations such as listing, completion, globbing, and prompt Git inspection in-process; only user commands and denv evaluation may spawn children.
- Keep execution and data structures flat; do not add an ish-owned AST or general configuration language.
- Preserve the interactive hot path by reusing buffers where practical and avoiding allocations on each keystroke.
- Use epsh for parsing and evaluation, but expose only syntax accepted by the README and `dispatch::plan()`.
- Refuse unsupported syntax before evaluation; do not pass background jobs, compound commands, functions, `source`, `eval`, or parameter operators to epsh.
- Run shell-state builtins in ish only as a whole input line; `cd` and `history` also have list forms.
- Register each ish-owned builtin once in `src/builtin.rs` with its execution class; route shell-state handlers through the private binary runtime in `src/shell.rs`.
- Keep interactive `Shell` private in binary `src/shell.rs`. Library `src/editor.rs` owns private editor state and returns typed actions from borrowed shell context; terminal, process, job and persistence effects stay in the binary.
- Keep at most one suspended foreground job.
- Prefer rustix for Unix and system calls; keep any libc exception narrow and explain it where it is used.
- Localize unsafe code at platform boundaries and preserve descriptor ownership and error handling.
- Update the README and relevant integration or PTY coverage when user-visible behavior changes.
- Keep dynamic command policy checks in epsh expansion and dispatch paths; ish refuses dynamic `eval`, `source`, `.`, and out-of-line shell-state builtins with status 2 before target effects.
- Keep arithmetic overflow behavior explicit: wrapping add, subtract, multiply, and negate; errors for zero division or remainder, signed division or remainder overflow, and invalid shift counts.
- Keep Rust workspace tests and the retained native byte-preserving conformance corpus in CI on supported hosts.
- Treat Lean theorems as claims about the model. Use the differential bridge and Rust tests for implementation correspondence and behavior.

## Topic map

- Open implementation tickets, accepted owner decisions, and next work:
  [TODO](TODO.md). Accepted RFCs are implementation contracts; their status
  does not claim the code is fixed. Keep the decision records after delivery.

- Build, test, installation, and release commands: [Development](docs/development.md).
- File operation behavior: [Filesystem](docs/filesystem.md). Native fixtures, required runs, and evidence limits: [Filesystem testing](docs/filesystem-testing.md).
- Filesystem measurements: [Benchmark harness](benches/fs/README.md) and [recorded results](benches/fs/results-linux.md).
- Execution flow, epsh integration, command routing, process boundaries, and module ownership: [Architecture](docs/architecture.md).
- History search ranking and session snapshots: [History search](docs/history-search.md).
- Completion matching, ordering, and hot-path constraints: [Completion](docs/completion.md).
- Design decisions: [capture and retirement](docs/rfc/ISH-32-capture-and-retire.md) and [release delivery](docs/rfc/ISH-42-release-delivery.md); all RFCs are in [docs/rfc](docs/rfc/). Dependency and data-structure decisions: [decisions](docs/decisions.md).
- PTY test environment and synchronization: [PTY tests](docs/ptytest.md).
- Lean project and bridge commands: [formal guide](formal/README.md),
  [protocol](formal/PROTOCOL.md), and
  [verification coverage](docs/verification/coverage.md).
- Verification details: [expansion generation](docs/verification/expansion-generation.md),
  [mutations](docs/verification/mutation-testing.md),
  [shift refinement](docs/verification/refinement.md),
  [capture convergence](docs/verification/capture-convergence.md), and
  [history-search measurements](docs/verification/history-search-profile.md).
