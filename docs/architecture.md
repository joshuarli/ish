# Architecture

ish is an interactive-only shell built around epsh. The implementation keeps
ish's user-facing syntax smaller than epsh's parser and runtime.

## Execution flow

1. The line editor returns an input line; aliases are expanded by the existing
   alias integration.
2. [`dispatch::plan`](../src/dispatch.rs) parses the line once with epsh and
   rejects constructs outside the supported subset. The accepted syntax is
   listed in the [README](../README.md).
3. A whole-line shell builtin is dispatched in ish. Other accepted programs are
   evaluated by epsh with ish's command policy and external-command handler.
4. The handler runs ish-owned builtins where needed and starts external
   commands with process-group and terminal control. ish synchronizes the
   resulting status and working directory.

The builtin registry assigns `cp` and `mv` the `Exec::Foreground` class. epsh
handles their list and redirection syntax, then calls ish's handler in the
original shell process. Explicit executable paths bypass the builtin registry
and keep the external utility available. The [file operation reference](filesystem.md)
defines their operands and filesystem policy.

The `Exec` class in [`builtin.rs`](../src/builtin.rs) defines where ish-owned
commands run. `InShell` commands must be the whole input line. `Hybrid`
commands such as `cd` and `history` run with shell state when alone and use the
stateless command path inside lists. `Native` commands can run in pipeline
stages. The registry is the source for builtin-name lookup and completion.

`epsh` supports more syntax than ish exposes. Keep the acceptance check in
`dispatch::plan()`; do not send unsupported syntax directly to epsh. Do not
add an ish-owned parser or general configuration language.

The epsh command policy checks expanded targets before builtin or function
dispatch. ish uses it to refuse dynamically selected `eval`, `source`, `.`, and
shell-state builtins outside their whole-line route. These refusals return
status 2 before the target runs or opens its redirections. The policy does not
undo effects from earlier commands or expansion.

ish also registers `cp` and `mv` with epsh's additive
`CommandPlacement::ParentProcess` API. The placement table owns command-name
bytes and is empty by default for other embedders. epsh captures its owner PID
when a `Shell` is created and carries that owner through evaluator forks. It
checks the expanded target, including nested `command`, before command-policy
callbacks, target redirections, or target dispatch. A pipeline or command
substitution is rejected with status 2 even when its evaluator path stays in
the current process. Argument substitutions finish before the handler runs.
See [epsh's process model](../crates/epsh/docs/process-model.md) for the API and
its limits.

## Filesystem operation boundaries

`src/fs_command.rs` parses ish's command options and adapts typed requests to
`ish-fs`. The library's contract covers path anchoring, mapping policy,
transfer, metadata, publication, and move retirement. It receives the shell
cwd, raw operands, environment-derived paths, and cancellation explicitly. It
does not change cwd or process environment.

The operation runs in the parent shell so redirections and list status use
epsh's normal evaluation. Workers must finish before the handler returns. The
foreground controller owns signal handling, cancellation, progress output,
worker event draining, and terminal restoration. `trash` uses the same
controller while keeping its separate command contract and root protections.
Workers receive explicit handles and options. They do not mutate shell state,
cwd, or process environment.

The engine contract uses descriptor-relative paths after resolving supplied
parents. It stages new objects on the destination filesystem and publishes a
new root only after its transfer completes. A merge publishes completed leaves
and can leave partial work. An isolated exact same-filesystem move renames
without walking descendants. Cross-filesystem moves require successful transfer,
required metadata, and durable destination publication before source retirement.
Retirement uses records of transferred objects and revalidation; it does not
recursively delete whatever later appears at a source path.

Stable source and destination trees are the full-correctness baseline. Path
anchoring and revalidation detect observed changes, but cannot make rename or
unlink conditional on both inode and mutation version against every concurrent
writer. The command does not claim protection from another process running as
the same account with authority over the trees.

Native validation requirements and observed limits belong to
[filesystem testing](filesystem-testing.md). Recorded measurements are in
[the benchmark report](../benches/fs/results-linux.md).

## Filesystem resources and hardlinks

The engine admits workers against descriptor and memory budgets, including
thread stacks, reused transfer buffers, and metadata captures. Budget constants
and admission checks live in [engine/mod.rs](../crates/ish-fs/src/engine/mod.rs).
These are allocation estimates, not measured peak RSS limits.
[The foreground controller](../src/foreground.rs) owns bounded event delivery,
progress coalescing, and signal polling.

[Dry-run](../crates/ish-fs/src/engine/dry_run.rs) enumerates each directory once,
charging retained names and frame capacities to its memory budget. It creates
no scratch state. Transfer, hardlink, and retirement records use unlinked spool
files. [Records](../crates/ish-fs/src/engine/records.rs) uses an in-spool AVL index
for logarithmic path lookup without extra descriptors. Private record and
retained-intent encoding is version 2; unknown versions are rejected.

Hardlink coordination keeps one active owner per source inode. Locks are
released before data transfer, synchronization, and event delivery. Regular
files and symlink objects share coordination; linking never follows a symlink.

Multi-source moves and merges record completed directory renames without
walking them. A later hardlink transfer indexes each recorded tree once. If
copied representatives already exist, the next directory is inspected for a
coupled group. A coupled tree uses transfer and guarded retirement. Reused
regular-file representatives are synchronized before durable retirement.
See [transfer.rs](../crates/ish-fs/src/engine/transfer.rs) and its
[repair regressions](../crates/ish-fs/src/engine/repair_tests.rs).

Transfer counter meanings are defined on
[Progress and Metrics](../crates/ish-fs/src/lib.rs). Logical cloned bytes and
streamed bytes do not measure physical I/O. Phase timings can overlap and must
not be added to estimate elapsed time.

## State and process boundaries

- Shell variables live in epsh's variable store. Child processes receive the
  exported values from that store; in-process command lookup and completion
  use its `PATH` value.
- A standalone `cd` runs in ish. For `cd` inside a list, epsh updates the
  command working directory; ish notices the resulting directory change and
  runs the directory-change hooks, including denv.
- Directory listing, completion, pathname expansion, and prompt Git inspection
  run in-process. External commands and command substitutions may start child
  processes. A `.envrc` that requests bash evaluation may also start one.

denv trust filenames use SHA-256 of canonical path bytes to keep storage names
bounded. Revocation removes both current and legacy approvals. The storage
boundary is implemented in [denv.rs](../src/denv.rs).

Evaluator fork, wait, callback, and cancellation behavior belongs to the
[epsh process model](../crates/epsh/docs/process-model.md).

Module entry points are [src/lib.rs](../src/lib.rs) and the interactive event
loop in [src/main.rs](../src/main.rs). Parser and evaluator internals belong to
[epsh's architecture](../crates/epsh/docs/architecture.md).

## Semantic verification

See the [formal guide](../formal/README.md) for semantic checks and their
limits, and [epsh testing](../crates/epsh/docs/testing.md) for Rust and native
corpus commands.

Shell terminology follows [epsh's architecture](../crates/epsh/docs/architecture.md).
