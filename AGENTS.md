ish is an interactive-only shell written in Rust. The README is the user-facing
behavior contract.

## Commands

- `make build` builds the shell.
- `make test` runs the library, integration, and PTY tests.
- `cargo check --workspace` checks the Rust workspace.

## Invariants

- Keep ish interactive-only; adding scripts, POSIX compatibility, `source`, `eval`, control flow, functions, background jobs, plugins, themes, or prompt configuration requires an explicit design decision.
- Keep shell-owned operations such as listing, completion, globbing, and prompt Git inspection in-process; only user commands and denv evaluation may spawn children.
- Keep execution and data structures flat; do not add an ish-owned AST or general configuration language.
- Preserve the interactive hot path by reusing buffers where practical and avoiding allocations on each keystroke.
- Use epsh for parsing and evaluation, but expose only syntax accepted by the README and `dispatch::plan()`.
- Refuse unsupported syntax before evaluation; do not pass background jobs, compound commands, functions, `source`, `eval`, or parameter operators to epsh.
- Run shell-state builtins in ish only as a whole input line; `cd` and `history` also have list forms.
- Register each ish-owned builtin once in `src/builtin.rs` with its execution class; route shell-state handlers through `src/main.rs`.
- Keep at most one suspended foreground job.
- Prefer rustix for Unix and system calls; keep any libc exception narrow and explain it where it is used.
- Localize unsafe code at platform boundaries and preserve descriptor ownership and error handling.
- Update the README and relevant integration or PTY coverage when user-visible behavior changes.

## Topic map

- Execution flow, epsh integration, command routing, process boundaries, and module ownership: [Architecture](docs/architecture.md).
- History search ranking and session snapshots: [History search](docs/history-search.md).
- Completion matching, ordering, and hot-path constraints: [Completion](docs/completion.md).
- PTY test environment and synchronization: [PTY tests](docs/ptytest.md).
