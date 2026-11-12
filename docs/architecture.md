# Architecture

ish is an interactive-only shell built around epsh. The [README](../README.md)
defines its smaller syntax and user-visible behavior.

## Execution flow

[main.rs](../src/main.rs) parses CLI arguments. Delegated `-c` commands replace
ish with `/bin/sh`; interactive execution enters [shell.rs](../src/shell.rs).
The private `Shell` owns the evaluator, editor, history, aliases, denv, input
reader, capture, and one suspended-job slot.

The editor returns a line, aliases expand, and [`dispatch::plan`](../src/dispatch.rs)
parses it with epsh. Planning rejects unsupported constructs before evaluation.
Whole-line shell commands run in ish; other accepted programs run through epsh.
The registry in [builtin.rs](../src/builtin.rs) owns execution classes, command
identity, lookup, completion names, and the finite builtin policy:

- `InShell` commands require the whole input line.
- `Hybrid` commands, including `cd` and `history`, also have list forms.
- `Foreground` handlers, including `cp`, `mv`, `rsync`, `l`, and `c`, run in the
  original process. Pipeline stages and command substitutions refuse them.
- `Worker` runs the linked fz CLI in a fresh same-binary executable, including
  pipeline and substitution placements. It installs no inherited host callback.

Planning and expanded dispatch reuse epsh's `command_action` parsing and raw
argument rules. Expanded targets are checked before target redirections,
assignments for dispatch, and callbacks; refusal returns 2. Earlier commands
and expansion effects are not rolled back. Unavailable epsh builtins cannot
fall through to PATH or implicit `cd`; explicit executable paths remain usable.
Standalone epsh and denv evaluators retain their unrestricted defaults.

`CommandPlacement::ParentProcess` stores owned command-name bytes and preserves
the creator PID across evaluator forks. It rejects nested `command` targets in
pipeline/substitution contexts even if that evaluator stays in the parent.
Argument substitutions finish before a parent handler runs. See the
[epsh process model](../crates/epsh/docs/process-model.md) for callback and fork
limits. epsh owns external processes, process groups, waits, and stopped-job
handles. ish owns terminal modes and the single suspended-job slot.

## Picker ownership

The [picker reference](fuzzy-pickers.md) records the native process policy and
shared-source revision. `pk` is an `InShell` command. It uses the shared fz
`Picker` with ish's `InputReader`, resize and termination events. Terminal
cleanup completes before batch validation and signaling. Process records and
OS handles remain private to [pk.rs](../src/pk.rs) and its native backends.

For `fz`, epsh stores an executable, fixed prefix and optional owned input
descriptor. Its ordinary external executor handles argv, environment, cwd,
redirections and jobs. Only the registered worker inherits that descriptor. Epsh relocates it away
from numbered redirections and appends its current number after the prefix.
The private binary entry loads no shell or config. The CLI adapter temporarily
claims the controlling terminal when a substitution needs it, then restores
the prior foreground group and signal state.

[fuzzy_input.rs](../src/fuzzy_input.rs) transfers editor-buffered bytes through
a seekable descriptor with a shared offset. Linux uses a memfd. macOS uses an
immediately unlinked private temporary file in `/tmp`. The worker reads one byte at a time before terminal input.
Unread bytes return before newer capture input. A suspended worker retains its
input session with the owned job. No transfer exists for an empty editor tail.
This descriptor is not candidate stdin and carries no selected output.

## Output capture

The debugging-only `?` command is gone. [capture.rs](../src/capture.rs) still
provides the PTY relay used by ordinary execution and suspended jobs, including
its bounded output-tail buffer. `copy-scrollback` copies the command session
log, not that buffer. Filesystem quarantine is a separate mechanism.

`Capture::begin` replaces terminal-bound standard descriptors with a PTY slave.
Only descriptors naming the same terminal as stdout are replaced, so explicit
redirections remain separate. `fg` resumes an existing session; `exit` does not
start one. The buffer allocation moves into the relay thread and back for reuse.

A single simple command converges stdin, stdout, stderr, and `/dev/tty` onto
the slave, with a separate child session and controlling terminal. The real
terminal becomes raw. The relay forwards input and output; only the slave line
discipline processes output. Its 200 ms size poll sends SIGWINCH only on an
actual size change, avoiding redraw feedback.

A converged child group is orphaned. The relay turns its suspend character into
SIGSTOP because macOS discards terminal-generated SIGTSTP for such groups.
Interrupt and quit use the slave line discipline. For in-process builtins with
no child foreground group, the relay signals the shell's group and ignores
suspend. Foreground-group queries use the master because macOS rejects the
slave query; group 0 means no owner. epsh clears its cached terminal descriptor
between converged commands, and diagnostics can reopen `/dev/tty` after a dead slave.

Pipelines and lists use split capture: stdout/stderr move to the slave while
stdin and the controlling terminal remain real. The relay mirrors slave input
modes with real-terminal output processing disabled, pausing while ish owns the
terminal. Programs that read the output terminal for input can hang in this
mode; use a single simple command for those programs.

`Capture::end` flushes stdout and restores replaced descriptors. Finishing its
`Session` stops and drains the relay and restores real-terminal modes. A stopped
job retains the session, with input forwarding paused until `fg`. Unwritten
input remains queued across full queues and pauses. After child exit, the tail
returns to the next prompt; after suspension it stays with the job.

Termination during external waits and fatal cleanup during descriptor replacement
have demonstrated gaps tracked in [TODO](../TODO.md). Prompt-only signal tests
do not establish the command-running behavior.

## Terminal integration

ish emits OSC 7 before prompts and OSC 133 at prompt/input, execution, and
completion boundaries. Cancellation, refusal, continuation, suspension, and
`fg` have distinct transitions. Repaints/resizes do not emit new prompt markers.
Markers stay on the interactive terminal stream rather than redirected output
or delegated `-c` execution. [The terminal matrix](terminal-interop-matrix.md)
separates byte-level PTY assertions from native-terminal observations.

## Filesystem operation boundaries

[fs_command.rs](../src/fs_command.rs) adapts `cp`/`mv` operands to `ish-fs` with
explicit cwd, raw paths, environment-derived inputs, and cancellation. Native
operations run in the parent under [foreground.rs](../src/foreground.rs), which
owns signals, bounded events, progress, workers, and terminal restoration.
Workers finish before handlers return and never mutate shell environment or cwd.
`rmrf` uses the controller with its own root protections.

`cp`/`mv` validate all operand mappings before mutation and use descriptor-relative
traversal after resolving supplied parents. New roots publish after transfer;
merges publish completed leaves and may leave partial work. Isolated exact
same-filesystem moves rename without walking descendants. Cross-filesystem
moves require transferred data, required metadata, and a durable destination
receipt before revalidated source retirement. They never delete an arbitrary
replacement tree found later at the source path.

`fdu-posix::identity` owns no-follow opens, mount identity, platform conversions,
and EINTR retries for copy/move and fdu deletion. `fdu-core` owns the portable
identity/kind values; `ish-fs` adds observations and transfer-version checks.
`fdu-posix::quarantine` owns their shared journal: nonblocking root locks,
synchronized intent before capture, synchronized captures before unlink,
and independent actual-removal versus synchronization outcomes.

Recovery validates owner, mount, ancestry, identity, and versioned records. It
restores names without overwrite and requires a fresh plan/retry, never replaying
deletion or publication. Stage records remain inspectable. Replacement records
include original source identity and destination parent/name so interrupted
exchanges remain inspectable instead of being repeated. Crossed mounts use
short-lived sessions at retained roots. Root locks own their iterators and use
creator PIDs to guard explicit unlock after fork.

Stable trees are the full-correctness baseline. Revalidation does not make rename
or unlink conditional on every mutation by another same-account writer.
[Filesystem behavior](filesystem.md), [native acceptance](filesystem-testing.md),
and [recorded measurements](../benches/fs/results-linux.md) own the detailed contracts.

[Native rsync](../crates/ish-fs/src/sync.rs) is a separate path-based engine
called by [sync_command.rs](../src/sync_command.rs). It does not inherit the
copy/move ancestry guards, anchored traversal, retirement receipts, or journal.
The [sync reference](filesystem.md#local-synchronization-rsync) records its
current destructive-boundary and deletion-error defects.

## Filesystem resources and hardlinks

[engine/mod.rs](../crates/ish-fs/src/engine/mod.rs) admits copy/move workers
against descriptor and allocation budgets, including stacks, buffers, and
metadata. These are estimates, not measured peak RSS limits. Dry-run charges
retained names and traversal frames, enumerates each directory once, and makes
no scratch state. Records use unlinked spool files with an AVL path index;
private encoding is v2. The shared journal writes v2 and has a restoration-only
v1 reader; unknown versions fail.

Whole-tree transfer holds the source directory instead of its parent, then
reopens `..` and verifies the saved parent before publication. This preserves
space for the independent quarantine lock within the 16-descriptor minimum.
Sources named `.` or `..` retain and budget their original parent descriptor.

One owner coordinates each hardlink inode; no coordination lock spans transfer,
synchronization, or event delivery. Regular files and symlink objects share
coordination without following links. Multi-source moves record whole-directory
renames lazily and index a renamed tree once when another hardlink needs it.
Trees coupled to existing representatives use transfer and guarded retirement;
reused regular-file representatives are synchronized before retirement. See
[transfer.rs](../crates/ish-fs/src/engine/transfer.rs) and its
[repair regressions](../crates/ish-fs/src/engine/repair_tests.rs).

[Progress and Metrics](../crates/ish-fs/src/lib.rs) define the counters. Logical
clone bytes and streamed bytes are distinct, neither measures physical I/O,
and overlapping phase timings must not be summed as elapsed time.

## State and process boundaries

Shell variables live in epsh's store. Startup imports the inherited environment
once; `SHELL`, `PWD`, and `OLDPWD` updates stay there. Children get explicit
exported environments and prefix assignments, retaining non-shell-name bytes.
The interactive binary passes store-derived HOME to config/history and derives
lookup, completion, and prompt identity from the store. Convenience library
constructors may read inherited HOME. Production never mutates host environment.

Standalone `cd` runs in ish; list-form `cd` changes epsh's cwd, which ish observes
before running directory hooks. Listing, completion, globbing, and prompt Git
inspection stay in-process. External commands, substitutions, and bash-selected
`.envrc` evaluation can spawn children.

[denv.rs](../src/denv.rs) restores a prior layer into a cloned variable store,
evaluates from its exported environment, validates changes, and replaces the
active record before publishing the store. Readonly/persistence failures retain
the current layer. Evaluation failure publishes the validated prior-layer
restoration and dirty metadata after clearing the old record. Nested inherited
state currently can lack restoration provenance; [ISH-89's draft](rfc/ISH-89-nested-denv-restoration.md)
proposes the missing ownership contract.

Active-record v2 encodes paths/value bytes in hex and distinguishes exported,
local, declared-but-unset, and absent bindings. Older records keep their escaped
text interpretation and restore exported bindings; only v2 is written. Atomic
replacement preserves the previous record on write failure. Trust filenames use
SHA-256 of canonical path bytes, and revocation removes current and legacy approvals.

Both evaluators reject retired file-loading helpers. `dotenv_if_exists` accepts
only the discovered approved `.env` assignments, including absence; final automatic
loading still wins over `.envrc` exports. Fixed-size failure records travel over
an evaluation-owned nonblocking pipe with two reporter descriptors at 10 or
higher, so redirecting one cannot hide failure. Any record rejects the candidate
regardless of final status. Bash polls failure/snapshot pipes under one deadline;
epsh checks after releasing owned children. Explicit sourcing still carries the
approved author's authority. Redefining helpers/reporters is possible because
this is a trust boundary, not a sandbox.

The library [editor.rs](../src/editor.rs) owns private line/mode/layout state and
reusable buffers. Borrowed `EditorContext` has aliases, history, cwd, HOME, PATH,
and directories, with no evaluator. Typed events/actions separate editing from
terminal polling, execution, persistence, and job effects in the binary.
Rendering writes to a caller-selected sink without flushing; resize takes
measured dimensions. Relative completion uses borrowed cwd without host `chdir`.
The current partial-path budget and edit-transition gaps are in [TODO](../TODO.md).

## Accepted boundary decisions

Retain the accepted RFCs as this repository requires: [builtin policy](rfc/ISH-24-builtin-allowlist.md),
[environment/editor ownership](rfc/ISH-15-environment-and-shell-ownership.md),
[foreground jobs](rfc/ISH-5-foreground-job-outcomes.md),
[callback process boundaries](rfc/ISH-8-callback-process-boundary.md),
[filesystem retirement](rfc/ISH-32-capture-and-retire.md),
[denv trust](rfc/ISH-26-denv-sourced-file-trust.md),
[input-tail ownership](rfc/ISH-62-capture-input-tail.md),
[login-shell delegation](rfc/ISH-63-login-shell-command.md), and
[release delivery](rfc/ISH-42-release-delivery.md). [TODO](../TODO.md) holds remaining
implementation and native acceptance work, including privileged macOS ownership.

## Semantic verification

The [formal guide](../formal/README.md) and [coverage matrix](verification/coverage.md)
distinguish model proofs, bounded Rust comparisons, and extracted shift refinement.
They do not prove terminal or kernel behavior. [epsh testing](../crates/epsh/docs/testing.md)
owns its Rust/native corpus commands; [epsh architecture](../crates/epsh/docs/architecture.md)
defines parser/evaluator terminology and internals.
