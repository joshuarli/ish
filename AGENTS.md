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
- PTY test environment and synchronization: [PTY tests](docs/ptytest.md).
- Lean project and bridge commands: [formal guide](formal/README.md),
  [protocol](formal/PROTOCOL.md), and
  [verification coverage](docs/verification/coverage.md).
- Verification details: [expansion generation](docs/verification/expansion-generation.md),
  [mutations](docs/verification/mutation-testing.md),
  [shift refinement](docs/verification/refinement.md), and
  [history-search measurements](docs/verification/history-search-profile.md).
