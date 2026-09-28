# Undo

Design notes for recoverable filesystem operations: what ish records, the
ordering rules that make recovery trustworthy, and the boundaries that keep the
feature small. User-facing behavior lives in the README's Undo section; this
document records what is easy to break while refactoring.

The implementation is the `crates/ish-undo` workspace crate plus a thin
adapter in `src/undo.rs`. The crate owns filesystem operations, storage,
journals, and replay. It must not depend on ish's parser, history, line editor,
or renderer: the shell hands it resolved paths, configuration, execution
identity, cancellation, and output handles (`Config`, `Io`, `Session`, `Txn`).
`src/undo.rs` owns dispatch, per-input transaction begin and end, the stopped
job hand-off, and the redirection-open provider. The crate's public surface is
what the shell uses; `ish_undo::testing` lists, item by item, the internals its
own tests, fixture, and benchmark reach into.

## Boundary

The contract is: **recover operations the shell itself performs.** Native `rm`,
`mv`, and writable redirections are recorded before they mutate anything.
Every other external command runs normally, is counted as uncaptured, and is
never labeled protected.

Deliberately out of scope, and not stubs awaiting a backend:

- checkpointing or diffing a tree around an arbitrary program (that is a
  second product: observed changes over an interval, not operations ish did);
- selective replay (`--only`) and forced conflict replacement (`--force`),
  which multiply the applied/unapplied/displaced states replay must handle;
- native cross-filesystem `mv`, which needs staging, recursive copy,
  publication, and source removal across filesystems. It is refused, nothing
  moves, and the message names `/bin/mv`. It must never silently fall back to
  an unprotected move;
- preload injection, Endpoint Security, privileged helpers, a daemon, FUSE,
  volume snapshots, or a filesystem watcher that pretends to capture old
  contents. Capture is rootless and synchronous.

Adding any of these is an explicit design decision (see `AGENTS.md`).

## Transactions

One accepted input is one transaction. `Session::begin` runs before expansion
and command substitution and only fills in memory; the journal, its directory,
and every store are allocated the first time something is recorded. Skipped
branches and read-only inputs create nothing.

Pipeline stages and command substitutions are forks of the shell, so an
inherited `Arc`, mutex, or buffered writer is not coordination. They share:

- an anonymous shared page, mapped once per shell before any fork, whose slots
  publish each transaction's allocated id and uncaptured-command count;
- `flock`s on descriptors each process opens itself (an inherited descriptor
  shares one lock), serializing id allocation and journal appends.

Descriptors are close-on-exec. No journal lock is held while waiting for a
command, terminal input, or an expensive copy.

The shell seals a transaction when its foreground job actually completes. A
stopped job keeps its transaction and slot active across `fg`. Sealing captures
post-state evidence for paths whose last performed action created or wrote
content, then appends the end record. That fold walks the journal once and
honors commit records: only operations that happened count, and an operation
with no commit is treated as performed. A removal of any size has no create or
write and costs one scan.

Lifecycle is derived, not stored: active (recording shell alive), completed
(end record), interrupted (no end record, shell gone), replaying (sealed, with
a replay run begun and unfinished while another process holds the replay
lock). Scope, preservation strength, and
execution state are separate facts; a successful exit does not prove complete
capture.

## Native operations

Native commands are dispatched from epsh's external handler after expansion, by
exact command name. An explicit path such as `/bin/rm` runs the external
utility; `command rm` is ordinary lookup and stays protected. Unsupported
options fail with status 2 rather than running an unprotected utility.
Operations refuse the filesystem root, `.`/`..`, and the undo store with its
ancestors, decided by a nonexecuting guard so tests never run a destructive
command against a real location.

`rm` preserves affected regular files before unlinking, preserves symlinks
themselves, records removed directories with their metadata, and walks with
bounded descriptors and depth. It does not rename a tree into a trash directory
and does not cross mount points. `mv` within one filesystem is a namespace
operation: it preserves only a replaced destination and records the rename; an
unchanged tree is never snapshotted. `rm -f` never waives preservation.

Redirections go through epsh's optional redirect-open provider, which owns the
actual `open` and returns the descriptor, so preservation happens on the same
verified object before truncation or writable exposure. Symlinks are resolved so
the target is protected without replacing the link. Non-regular targets
(`/dev/null`, FIFOs) keep normal behavior and are never read to snapshot.
Appends preserve a complete pre-image, since restoring a length is unsafe after
unrelated writes.

## Preservation

Strength is explicit, never hidden behind a generic copy:

| Kind | Mechanism | Contract |
|---|---|---|
| clone | `fclonefileat` (macOS), `FICLONE` via rustix (Linux) | frozen; no data moves |
| linked | hard link retaining the inode | only for unlink/replace; another link or an open writer can still change it |
| copy | independent byte copy | frozen; bounded by the copy limit and free-space reserve |

Capability is probed per filesystem by trying it, and a filesystem name is
never proof. Capability errors (`ENOTSUP`, `EXDEV`, `EINVAL`, and so on) select
the next strategy; permission, I/O, durability, and space errors fail the
preservation and are never reclassified as "unsupported". A write needing a
frozen pre-image that cannot get one is refused. A hard link is never used for
an in-place-write backup, `st_nlink == 1` is never treated as immutability, and
a linked backup is never chmod'd or chown'd.

Preservation records raw path and symlink bytes, modes, timestamps, ownership,
xattrs, BSD flags, and (macOS) ACLs and resource forks where an unprivileged
user can restore them. Apple clones have ownership, set-ID, and ACL exceptions,
so the clone is followed by an explicit metadata copy. Limits are reported
through notes rather than promised away. Restoring rejoins hard links within a
restored set and never modifies aliases outside it.

Restoring a frozen version goes through a new clone or copy; the stored object
is never exposed as a live file.

## Storage and journal

Layout is documented at the top of `src/store.rs`. The catalog and
home-filesystem objects live in `~/.local/share/ish/undo` (ish's convention,
independent of command history). Clones and hard links only work within one
filesystem, so other filesystems get an explicitly registered private volume
store (`<dir>/.ish-undo`). Each volume transaction directory carries an `owner`
file so associations can be rebuilt without the catalog; the catalog and a
volume never commit atomically together, so unreferenced objects are orphan
candidates, not garbage. A missing volume is unavailable, never evidence of
deletion. Object names are opaque, never user paths.

A journal is `ISHUNDOJ`, a version, then length-framed, CRC-32-checked records
with LEB128 integers and length-prefixed byte strings, so paths are lossless.
A reader stops at the first record that fails validation, and the next appender
truncates a torn tail under the lock. Changing an encoding incompatibly bumps
the journal version; readers refuse other versions rather than guess.

**Ordering invariant:** the saved object and the record needed to recover it
are durable before the destructive mutation is permitted. Prepare records are
synced; commit records are not, because an unsynced commit is reconstructed
from the filesystem as an ambiguous operation and never assumed. Recursive
removal batches preparation so one journal sync and one object-directory sync
cover many entries.

Process-crash recovery is the tested guarantee. Power loss is best effort: on
macOS `fsync` does not flush the drive cache, and filesystems without ordering
guarantees can lose recent records. Recovery never invents success, never
silently deletes the only saved version, and never overwrites live files.

## Replay

A transaction's journal folds into a model of its actions, whether each
happened, and how replay runs moved each one. Undo walks actions in reverse,
redo forward. Redo applies recorded state, not the original command.

Replay is conservative. Before every step it checks the current filesystem:

- restore a deletion only into an unoccupied destination;
- remove or replace a created or modified object only while it matches the
  recorded post-state (compared against a frozen post-image where one was
  taken, never by size and mtime alone);
- reverse a rename only while the moved object is still there.

Anything else is a conflict: reported, left untouched, with everything that can
be undone still undone. The user resolves it and retries the transaction by id;
completed steps are skipped. Filesystem races with unrelated writers are not
solved by advisory locks; detected uncertainty is a conflict, never permission
to overwrite.

**Steps are write-ahead.** A step that removes or replaces something first
preserves the current version and syncs its objects, then appends a
`StepPrepare` record naming the displaced version and, for restores, the
staging entry it created with that entry's identity. Only then does it mutate,
and only afterwards is `StepDone` appended. A step that ends without changing
anything after preparing writes a settled `StepConflict`. An unfinished step
(prepared, never done or settled) is reconciled on the next run:

- a recorded staging entry is removed only while it is still that object
  (device, inode, kind). A name prefix is not proof of ownership;
- if the step's goal state is found already in place, the displaced version
  comes from the prepare record instead of being recorded as none, so redo
  still has it;
- a conflict found without a prepare of its own does not settle an older
  interrupted attempt.

Staging entries are hidden files named
`.ish-undo-<txn>-<run>-<step>-<random>`, created exclusively (`O_EXCL`,
`symlinkat`, `linkat`, or a clone/copy that refuses to overwrite) and never
reused. The identity is journaled as soon as the entry exists and before any
data is copied into it. An entry that already has the chosen name is never touched; the
step picks another name. A crash can leave a staging entry behind until the
transaction is retried.

## Locking and retention

Two locks matter beyond the append lock:

- the home lock serializes id allocation, collection, and the volume registry;
- `txn/<id>/replay.lock` is held for a replay's whole run and for any deletion
  of that transaction. It is created with the transaction and opened without
  `O_CREAT` everywhere else, so a lock file cannot reappear inside a directory
  being deleted.

Collection and purge both take the replay lock before reading the journal that
justifies deleting, so they cannot remove a journal or objects from under a
replay, whichever transaction the replay was asked to act on. Collection deletes whole
transactions, oldest first, and never collects active, interrupted, replaying,
or unavailable-volume transactions or the newest completed one.

Automatic maintenance is bounded by the work it does, not by how much it
deletes. A pass may load a fixed number of journals (and remove a fixed number
of orphaned volume directories), resumes after the last transaction it handled
so protected transactions at the old end cannot starve collectable ones, and
continues at the next command boundary. Totals come from a small `size` file
per transaction, recording logical bytes and whether the journal was sealed, so
a pass reads one tiny file per transaction instead of opening journals or
walking object directories. Rules that keep that cache honest:

- it is written only while holding the replay lock (at seal, and by
  maintenance when it measures a transaction), never for an active journal;
- replay drops it before appending anything;
- a cached "never sealed" state means interrupted or unreadable, which nothing
  collects, so those cost no journal load to skip.

Unreadable journals (including other versions) are kept for inspection and
removed only by an explicit `undo purge`. Sizes are reported as logical bytes,
allocated blocks (an overestimate for clones), and bytes copied in userspace;
exclusive copy-on-write usage is not knowable, and is not claimed.

## Testing

The suites use real temporary filesystems rather than mocks, and each
mutation test or benchmark gets a fresh private fixture root with separate
work, HOME, and store children and an explicit environment and cwd. Cleanup is
limited to validated harness-owned directories and never follows symlinks. No
test needs sudo or touches real root, home, or worktrees.

- `ish-undo-fixture` (`crates/ish-undo/tests/support/fixture.rs`) runs one
  operation per process so a test can crash it at a named boundary and recover
  in a fresh process, hold descriptors and mappings open across a preservation,
  or pause a replay while holding its lock.
- Fault injection is test-only plumbing in `src/fault.rs`, driven by
  `--fault point[:skip]=errno|abort|block`. Points include `journal-append`,
  `journal-appended`, `prepared`, `clone`, `link`, `copy`, `write`, `sync`,
  `rename`, `unlink`, `rmdir`, and the replay points `replay-prepared` (prepare
  durable, nothing mutated), `replay-mutated` (mutated, completion not yet
  recorded), and `replay-step` (completion recorded). Crash windows belong on
  both sides of a mutation, not only after the record.
- Use readiness handshakes rather than sleeps.
- PTY scenarios in `tests/pty.rs` cover the shell path: aliases, globs,
  pipelines, redirection order, Ctrl+C, and stop/resume.

Tests that need a second filesystem use `ISH_UNDO_TEST_VOLUME`, a writable
directory on a different filesystem (the Linux container mounts a tmpfs) and
skip explicitly without it. Capability-specific assertions establish that the
intended backend actually ran, and clone-dependent ones are guarded by a probe.

Linux validation reuses the repository Dockerfile and `musl-cargo`: build from a
copy of the sources in container-owned storage (sources mounted read-only), run
permission-sensitive suites as an unprivileged UID, mount a tmpfs for the second
filesystem, and report native reflink validation separately from the
hard-link and copy fallbacks, since the container filesystem decides which are
reachable.

## Benchmarks

`cargo bench --bench undo` prints a one-shot report before the timed runs:
entry counts, preservation backends, bytes copied in userspace, and where time
went. Protected removal is timed as the shell runs it (removal, seal, and the
command-boundary maintenance pass) at 1,000 and 10,000 entries, and at 100,000
with `ISH_UNDO_BENCH_LARGE=1`. Read the seal column against entry count: it
should grow linearly. The large and sparse file rows are labeled separately,
because a sparse file's timing says nothing about data movement. There are no
absolute timing thresholds; validation reads stay outside timed regions.
