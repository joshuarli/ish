# Undo recovery

The `ish-undo` crate owns protected filesystem operations, transaction storage,
journals, preservation, and replay. [`src/undo.rs`](../src/undo.rs) connects it
to the interactive shell. User commands and settings are documented in the
[README](../README.md).

## Recording boundary

One accepted input line is one transaction. The transaction starts before
expansion and command substitution, so those operations, pipeline stages, and
writable redirections share it. A stopped foreground job keeps its transaction
until it finishes after `fg`.

The shell records its native `rm` and `mv` after expansion, plus writable
redirections through epsh's redirect-open provider. Other external programs
run normally and are marked as uncaptured; changes they make internally are
outside the recovery boundary. Dispatch is by exact command name: `command rm`
is protected while an explicit path such as `/bin/rm` is external. A
cross-filesystem native `mv` is refused; it would require a copy followed by
source removal.

Undo state changes are refused in pipeline stages and command substitutions.
Read-only commands such as `undo list` and `undo show` can run there.

## Invariants

- Preserve the affected version and durably record the operation before a
  destructive mutation. If preservation fails, fail closed without mutation.
- The redirect-open provider performs the actual open. It must preserve the
  same verified target before truncation or writable exposure; symlinks protect
  their targets, and appends preserve a full pre-image.
- Prefer a frozen clone or copy for in-place writes. A retained hard link is
  allowed for unlink or replacement, where it preserves the inode, but must not
  be treated as an immutable snapshot.
- Replay checks the current filesystem before each step. A conflict is reported
  and left untouched; retry skips completed steps.
- Before replay removes or replaces anything, it preserves the displaced
  version and writes a durable prepare record. Any staging path is recorded
  with its object identity; a matching name alone is not proof of ownership.
- Pipeline stages and command substitutions are separate processes. Coordinate
  through process-shared transaction state and locks opened independently by
  each process, not through inherited buffers or file locks.
- Do not hold a journal lock while waiting for a command, terminal input, or a
  long copy.

Preservation capability is probed on the destination filesystem. Unsupported
clone operations may fall back to another preservation method; permission,
space, I/O, and durability failures must remain errors. Never remove the only
saved version or overwrite a live file during recovery.

## Storage and recovery

A registered volume has a private `.ish-undo` store. A disconnected volume is
unavailable, not evidence that its transactions were deleted.

The journal format is versioned and checksummed. Readers stop at an invalid
tail; the next writer truncates that tail while holding the append lock.
Readers must refuse an unknown journal version rather than guess at its
encoding. See [`journal.rs`](../crates/ish-undo/src/journal.rs) and
[`store.rs`](../crates/ish-undo/src/store.rs) for the implementation.

Home-store maintenance serializes id allocation, collection, purge, and the
volume registry. Replay and transaction deletion share a per-transaction lock,
so collection cannot remove data from beneath a replay. Collection works oldest
first and keeps active, interrupted, replaying, unavailable-volume, and newest
completed transactions. Automatic collection is bounded and resumes at a later
command boundary.

Process-crash recovery is covered by tests. Power-loss durability is best
effort: filesystem and drive-cache ordering is not guaranteed on every
platform. A transaction with a torn final journal record is recoverable from
the valid prefix; an operation whose completion was not recorded must be
reconciled against the filesystem, never assumed to have failed.

## Tests

The undo crate's operation, journal, and replay tests use isolated filesystem
fixtures and fault points around mutation boundaries. Run them with
`cargo test -p ish-undo`. Shell routing and terminal behavior are covered by
[`tests/integration.rs`](../tests/integration.rs) and
[`tests/pty.rs`](../tests/pty.rs).
