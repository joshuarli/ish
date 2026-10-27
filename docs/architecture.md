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
   evaluated by epsh with owned command rules and a named native dispatcher.
4. epsh starts ordinary external commands and owns their groups and waits.
   The native dispatcher handles ish-owned operations in the original process.
   ish synchronizes the resulting status and working directory.

The builtin registry assigns `cp` and `mv` the `Exec::Foreground` class. epsh
handles their list and redirection syntax, then calls ish's handler in the
original shell process. `l` and `c` use the same execution class.
Explicit executable paths bypass the builtin registry
and keep the external utility available. The [file operation reference](filesystem.md)
defines their operands and filesystem policy.

The `Exec` class in [`builtin.rs`](../src/builtin.rs) defines where ish-owned
commands run. `InShell` commands must be the whole input line. `Hybrid`
commands such as `cd` and `history` run with shell state when alone and use the
stateless command path inside lists. `Foreground` handlers require the
original process and refuse pipeline stages and command substitutions. The registry is the source for builtin-name lookup and completion.

`epsh` supports more syntax than ish exposes. Keep the acceptance check in
`dispatch::plan()`; do not send unsupported syntax directly to epsh. Do not
add an ish-owned parser or general configuration language.

The finite interactive builtin policy lives in `builtin.rs`. Its names drive
planning, epsh's opt-in builtin allowlist, completion, and command lookup; its
owned argument rules restrict `set` to listing and named-variable assignment.
Planning and expanded dispatch reuse epsh's `command_action` parser to check
every nested execution target. `CommandRule::validate_arguments` accepts raw
argv for both checks. Expanded targets are checked before their redirections,
assignments for dispatch, or execution, and refusals return status 2. The policy
does not undo effects from earlier commands or expansion. Lookup can identify
whole-line ish commands without granting permission to invoke them through
`command`. Unavailable epsh names cannot fall through to PATH lookup or implicit
directory changes; explicit executable paths retain their normal behavior.
Standalone epsh shells and denv evaluators retain their unrestricted defaults.

ish registers native handler commands, including `cp`, `mv`, `l`, `c`, and
list-form `history`, with epsh's additive
`CommandPlacement::ParentProcess` API. The placement table owns command-name
bytes and is empty by default for other embedders. epsh captures its owner PID
when a `Shell` is created and carries that owner through evaluator forks. It
checks the expanded target, including nested `command`, before command-policy
callbacks, target redirections, or target dispatch. A pipeline or command
substitution is rejected with status 2 even when its evaluator path stays in
the current process. Argument substitutions finish before the handler runs.
See [epsh's process model](../crates/epsh/docs/process-model.md) for the API and
its limits.

Ordinary external programs use epsh's default executor. The named external
dispatcher handles native ish operations in the parent process. epsh owns
process groups, child waits, output relays, and stopped-job handles. ish owns
the single suspended-job slot, terminal modes, and terminal output capture.

## Output capture

[`capture.rs`](../src/capture.rs) saves the last command's terminal output for
`?`. Around every accepted line except `?`, `fg`, and `exit`, `Capture::begin`
replaces the shell's own terminal-bound standard streams with the slave side
of a new pseudo-terminal. Replacing fds in the shell process covers builtins,
epsh pipeline stages, and external children without touching the spawn path.
Redirections inherit the substitution, so only bytes that would have reached the
terminal are saved. Each of fds 0, 1, and 2 is replaced only when it names the
same terminal as fd 1 was.

A single simple command converges onto the slave: stdin joins stdout and
stderr there, the slave starts as a copy of the real terminal's modes, and the
foreground child takes the slave as its controlling terminal in a session of
its own. Every descriptor the child can open for keyboard input, including
`/dev/tty`, resolves to the same terminal, so interactive programs work as
they do under other shells. While such a command runs, the real terminal sits
in raw mode and a relay thread copies output from the master to the real
terminal, appending it to a bounded `OutputBuffer`, and forwards typed input
from the real terminal to the master. It polls every 200 ms for window size
changes and applies them to the slave, signalling `SIGWINCH` to the slave's
foreground group only when the size actually changed: an unconditional signal
would make a fullscreen child redraw on every poll, and each redraw would wake
the relay again. Output is processed once, by the slave's line discipline;
the raw real terminal passes those bytes through unchanged.

Keyboard signals need relay help because a converged child is orphaned (its
session has no link to the shell's), and the kernel discards
terminal-generated stops for orphaned groups on macOS. The relay consumes the
slave's suspend character and sends `SIGSTOP` — never `SIGTSTP`, which would
be discarded — to the slave's foreground group. Interrupt and quit characters
reach a converged child through the slave line discipline, but an in-process
builtin owns no foreground group, so for those the relay signals the shell's
own group instead, mirroring what split capture's real terminal would do;
suspend stays ignored there. The foreground group is read through the master
with raw libc: macOS rejects the query on the slave side, and a fresh slave
reports pgid 0 (rustix turns that into a failed assertion, so 0 means none).
Spawning a converged child also clears epsh's cached terminal descriptor,
which would otherwise point at the previous command's dead slave, and shell
diagnostics fall back to `/dev/tty` when stderr itself points at such a dead
capture terminal.

Pipelines and lists keep the split capture: only stdout and stderr move to the
slave, stdin stays on the real terminal, and the relay mirrors the slave's
mode changes that affect input back onto the real terminal with output
processing still disabled there. Mirroring pauses while the shell owns the
terminal, such as while a job is stopped. The split keeps job control and the
controlling terminal untouched, which pipelines need because their stages
cannot share one controlling terminal. It cannot serve programs that read the
output terminal for keyboard input; those hang under the split, so prefer a
single command for fullscreen programs.

`Capture::end` flushes Rust's stdout and restores the replaced fds; the caller
then finishes the `Session`, which stops the relay, drains the master, and
returns the buffer. The relay restores the real terminal's startup modes as it
exits. A command that stops keeps its `Session` in the `Job`, because the job
still holds the slave; the relay pauses input forwarding until `fg` resumes
the job, so prompt keystrokes are not forwarded into the suspended child. `fg`
finishes the session when the job ends. The buffer moves into the relay thread
and back, so its allocation is reused across commands without sharing.

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
Retirement uses records of transferred objects and revalidation. It does not
recursively delete whatever later appears at a source path.

`fdu-posix::identity` owns descriptor-relative identity, no-follow opens,
mount identity, platform conversions, and EINTR retries for all three engines.
`fdu-core` retains filesystem-independent `FileIdentity` and `EntryType` values.
`ish-fs` retains its richer `Observation` and transfer-version comparisons.

`fdu-posix::quarantine` owns the shared journal for `fdu-delete` and `ish-fs`.
Its owned retirement records contain ancestor identities and an expected object
and kind. It locks each recovery root nonblockingly, synchronizes intent
publication before capture, and synchronizes captured names before unlink.
The caller authorizes unlink after its source and durable-destination checks.
The journal reports actual removal separately from synchronization errors.
Recovery restores names without overwrite and requires an explicit retry.
Stages have a separate record kind and remain inspectable after interruption.
The v2 replacement record kind adds the original source identity and destination
parent/name to the displaced-object intent. The shared primitive publishes and
syncs that record before invoking atomic publication, then uses the same
capture, verification, unlink, and restoration steps. Recovery distinguishes an
unchanged source from an exchange interrupted before capture and retains the
latter for inspection. It never replays publication. Readers that do not
recognize the record kind reject it.

Traversal checks source and existing destination directories as they are
reached. A crossed mount uses a short-lived session at its retained mount root
for each retired descendant; sessions are not accumulated by mount count.
The independently opened root lock owns its directory iterator, avoiding a
second descriptor during ordinary recovery-root enumeration. Its owner PID
guards explicit unlock across fork inheritance.

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
for logarithmic path lookup without extra descriptors. Private spool encoding
is version 2. Durable quarantine records use the shared v2 writer and a
restoration-only v1 reader. Unknown versions are rejected.

During whole-tree transfer, the held source directory replaces its parent
descriptor. Before publication, the engine reopens `..` through that directory
and requires the saved parent identity. This leaves room for the independent
quarantine root lock within the 16-descriptor minimum budget. Sources named `.`
and `..` retain their original parent descriptor. Admission charges that extra
descriptor and refuses the operation when the budget cannot fit it.

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

- Shell variables live in epsh's variable store. Startup imports the inherited
  environment once and assigns `SHELL` and `PWD` there. Directory changes update
  `PWD` and `OLDPWD` there. No production path mutates the host environment;
  the public process-environment mutation wrappers have been removed.
  Child processes receive exported store values and prefix assignments through
  explicit environments; inherited non-shell names retain their bytes.
  In-process lookup, completion and prompt identity use store values.
  Config and history convenience constructors can read inherited `HOME`; the
  interactive binary supplies its store-derived home through explicit inputs.
- A standalone `cd` runs in ish. For `cd` inside a list, epsh updates the
  command working directory; ish notices the resulting directory change and
  runs the directory-change hooks, including denv.
- Directory listing, completion, pathname expansion, and prompt Git inspection
  run in-process. External commands and command substitutions may start child
  processes. A `.envrc` that requests bash evaluation may also start one.

Denv receives a mutable epsh shell and uses its actual cwd for discovery. It
restores the prior layer into an owned variable-store candidate, evaluates from
that candidate's exported environment, and validates all resulting changes on a
clone before publishing anything. The matching active record is persisted before
the store is replaced. Readonly or persistence errors retain the current layer.
Evaluation failure publishes only the deliberate prior-layer restoration and
dirty metadata, after validating and clearing the old active record.
`on_startup` and `on_cd` return `Result<(), String>`; `command` returns a status.
There is no public delta for callers to apply partially.

Active-record version 2 encodes project paths and prior value bytes in hex, with
explicit exported/local tags for assigned and declared-but-unset bindings,
and a separate absent-binding tag. Version 1 and unversioned
records retain their historical escaped-text meaning and restore their values
as exported bindings. Only version 2 is written. Atomic replacement keeps a
failed write from replacing a previous restoration record.

denv trust filenames use SHA-256 of canonical path bytes to keep storage names
bounded. Revocation removes both current and legacy approvals. The storage
boundary is implemented in [denv.rs](../src/denv.rs).

Both denv evaluators install rejecting stubs for the six retired file-loading
helpers. An evaluation-owned nonblocking pipe carries fixed-size helper records
through epsh forks and bash subprocess contexts. Each helper reports through two
independently owned descriptors allocated at 10 or higher. A temporary redirection
of either reporter cannot hide the failure on the other. Bash inherits the
allocated descriptors without reserving a fixed failure descriptor.
A record invalidates the whole
candidate independently of its final status. Bash polls the failure pipe beside
its environment snapshot pipes under the same deadline. epsh checks the pipe
after its evaluator releases owned children. Both evaluators receive explicit
working environments; epsh uses `ShellBuilder::environment`, while bash uses
`Command::env_clear` and explicit entries.
The trust boundary covers the discovered, approved bytes. Explicit sourcing by
approved code carries the author's authority without denv approval of its targets.
Trusted code can deliberately replace both reporters or redefine the rejecting
helpers. The failure channel does not sandbox approved code.

Evaluator fork, wait, callback, and cancellation behavior belongs to the
[epsh process model](../crates/epsh/docs/process-model.md).

The binary entry point [main.rs](../src/main.rs) parses CLI arguments and enters
[shell.rs](../src/shell.rs). Its private `Shell` owns the evaluator, denv,
history, aliases, suspended job, capture and input reader. Terminal polling,
raw mode, output flushing, persistence and command execution remain there.

The library [editor.rs](../src/editor.rs) owns the line, mode, history navigation,
layout and reusable completion/search/path buffers behind private fields.
`EditorContext` borrows aliases, history, cwd, HOME, PATH and recent directories;
it contains no interactive shell or evaluator. `EditorEvent` carries key, paste
and measured resize input. `EditorAction` requests submission, cancellation,
exit, completion, history sync/search, directory changes or a layout dump.
`write_output` writes buffered rendering to a caller-selected sink without
flushing the terminal. Resize does not perform ioctls. Relative completion uses
the borrowed cwd and leaves the host cwd unchanged. Dispatch and action tags
add no per-event heap allocation; existing strings and completion/search buffers
are retained across prompts.

[mode.rs](../src/mode.rs) is private library state. [external.rs](../src/external.rs)
implements native dispatch; epsh owns external spawning and job waits.
[diagnostics.rs](../src/diagnostics.rs) renders layout dumps and writes them with
collision-only retries; other open errors preserve their actual path and cause.
[cd_rewrite.rs](../src/cd_rewrite.rs) owns `cd` history rewriting. Parser and
evaluator internals belong to [epsh's architecture](../crates/epsh/docs/architecture.md).

## Accepted boundary decisions

Accepted records remain the contracts for the boundaries below. [TODO](../TODO.md)
tracks implementation status and native verification limits:

- [ISH-24](rfc/ISH-24-builtin-allowlist.md): one finite interactive builtin
  policy for planning, expanded argv, completion, and lookup.
- [ISH-15, ISH-52 and ISH-48](rfc/ISH-15-environment-and-shell-ownership.md):
  explicit environments, a private binary `Shell` in `src/shell.rs`, and a
  library editor seam in `src/editor.rs`.
- [ISH-5](rfc/ISH-5-foreground-job-outcomes.md) and
  [ISH-8](rfc/ISH-8-callback-process-boundary.md): epsh-owned tracked jobs and
  explicit callback process boundaries.
- [ISH-32](rfc/ISH-32-capture-and-retire.md): shared journaled quarantine and
  identity primitives, with conservative recovery rather than resumed deletion.

The Linux release policy is implemented. Its native macOS panic probe remains
tracked under [ISH-45](../TODO.md).

## Semantic verification

See the [formal guide](../formal/README.md) for semantic checks and their
limits, and [epsh testing](../crates/epsh/docs/testing.md) for Rust and native
corpus commands.

Shell terminology follows [epsh's architecture](../crates/epsh/docs/architecture.md).
