# ISH-32: Shared capture and retirement

Status: accepted; implementation pending (2026-10-22)

## Decision

Use **fdu-delete's journaled quarantine protocol** for destructive retirement in
both `fdu-delete` and `ish-fs`. Extract its OS primitives and journal into a small
`crates/fdu-posix` crate. Do not retain ish-fs's independent sibling
`.ish-fs-retired-*` / `.ish-fs-intent-*` protocol, and do not add a background
sweeper or a crash-resume transaction engine.

The existing implementation in
[`quarantine.rs`](../../crates/fdu-delete/src/posix/quarantine.rs) supplies the
ordering and conservative recovery policy. Extraction must generalize its
`Tree` / `NodeId` inputs into owned records; callers retain their scan, transfer,
and user-policy decisions. This is an accepted target design, not a claim that
current `cp` / `mv` recover after a crash.

## Ownership and identity

- Keep `fdu-core` filesystem-independent. Its `FileIdentity` and `EntryType`
  remain the shared value types; `fdu-posix::identity` owns descriptor-relative
  observations, mount identity, no-follow opens, checked platform conversions,
  and the shared EINTR helper. All three filesystem engines use that module.
- Remove ish-fs's competing identity representation without dropping its
  kind checks. Its richer `Observation` and source-version comparisons remain
  transfer policy. Device/inode equality alone never authorizes source
  retirement after copying.
- Preserve the stronger destructive mount checks: Linux mount IDs and macOS
  filesystem/mount identity. Centralize the macOS `fsid_t` boundary once. An
  unavailable required identity is an error, not a device-number-only fallback.
- `fdu-posix::quarantine` owns session locks, random names, versioned bounded
  records, capture, restoration, synchronization, and recovery. It must not
  depend on the scanner, transfer scheduler, or shell. Use direct concrete APIs,
  not an extensible storage framework.

## One protocol

An operation uses an anchored, locked recovery root on each filesystem whose
entries it will retire. The root must stay outside the tree retired on that
filesystem. For a normal source operand this is its parent; the existing fdu
scan root remains suitable. A crossed nested mount needs its own root and
session. If such a root cannot be established safely, retain the affected source
and report partial completion. Never move captures across mount boundaries.

Acquire root locks nonblockingly and retain them through recovery and mutation;
an occupied root produces a diagnostic rather than waiting while holding other
roots. Create a mode-0700 session directory using at least 128 bits of OS
randomness and exclusive creation with bounded collision retries. A PID and
counter are not an ownership token. Persist the session header and sync the
session and root directories before the first capture. Validate that header,
ownership, and directory type before trusting a session found during recovery.

For each entry, use this order:

1. Validate the held parent, its ancestry/mount, and the caller's expected
   source observation. Persist a bounded versioned intent, including original
   name, ancestor identities, expected object/kind, and capture name. Sync the
   record and the directory that publishes it before renaming anything.
2. Rename the source into the session with no replacement. Sync the original
   parent and quarantine directory. Re-observe the captured object and check
   identity and kind, plus the caller's required version checks. Account only
   for metadata transitions caused by the engine's own rename/hardlink work;
   do not discard source-version checks to accommodate the shared primitive.
3. Only then unlink the verified capture. For cross-filesystem `mv`, a current
   durable-destination receipt and its destination revalidation remain required
   immediately before retirement. The shared journal cannot manufacture that
   authorization.
4. Sync the affected namespace before removing the intent; sync intent removal
   too. On mismatch, cancellation, or a pre-unlink failure, restore using
   no-replace rename and sync both directories. A conflicting original name
   leaves the capture and record intact and names them in the diagnostic.

Directories retire bottom-up and must be empty. Preserve ish-fs's hardlink,
metadata/version, destination-publication, and partial-completion rules. A
successfully removed source whose directory sync fails is still reported as
removed with a durability failure; it must not be described as retained.

## Recovery and migration

Check known sessions in participating recovery roots before planning new
destructive work. Recovery restores a recorded capture to its original name
only after validating the record, root, ancestry, mount, and captured object;
it never overwrites a live name. If validation is ambiguous, retain and report
the objects. Successfully restored entries require a fresh scan/plan and an
explicit retry; recovery must not silently continue a stale deletion plan.
Records for an already absent capture can be cleared only after validating
their provenance and the relevant namespace. Recovery never resumes unlinking
merely because the old command was `rm` or `mv`.

Keep a reader for existing `.fdu-quarantine-v1-*` sessions during migration.
Old records lack the new expected-object fields: use their existing conservative
no-overwrite restoration rules and do not infer permission to delete anything.
Malformed, unknown-version, or unrelated lookalike entries remain untouched and
block destructive work in that root with their exact paths.

Staging is not source retirement. Register new staging objects in the same
session ownership scheme, using a distinct record kind. After a crash, retain
and report unpublished stages; do not guess whether copied bytes should be
published, deleted, or used to finish a move. Published destinations remain in
place even when a captured source is restored, so recovery may leave duplicates.
This provides recovery of names, not rollback of an entire copy or move.

At operation start, also report existing legacy `.ish-fs-stage-*`,
`.ish-fs-retired-*`, and `.ish-fs-intent-*` entries in the participating parents.
There is no automatic migration or deletion of those unjournaled objects. A
prefix alone never proves ownership. Limit discovery to operation roots and
parents reached by the normal traversal; do not scan the user's filesystem.

## Implementation and acceptance

1. Extract the shared identity/mount primitives and migrate their callers.
2. Extract and version the fdu journal, retaining its old-format reader and
   recovery tests. Make the new protocol pass those tests before moving ish-fs.
3. Migrate source retirement and displaced-entry cleanup; then register staging
   ownership and remove the duplicate retirement/identity code. Update
   [Filesystem](../filesystem.md) and its testing guide only with verified
   behavior. Ordinary copies that do not replace entries never retire sources.

Exercise real subprocess termination at intent publication, capture, unlink,
and each synchronization/record-cleanup boundary; injected I/O errors alone do
not demonstrate restart recovery. Cover repeated recovery, name conflicts,
corrupt/unknown records, legacy sessions, parent/mount substitution, hardlinks,
random-name collisions, and concurrent invocations. Verify that an interrupted
cross-filesystem move preserves its published destination and never retires a
source on replay. Run the native Linux and macOS filesystem suites, and record
the fsync/throughput cost without weakening ordering to regain benchmark speed.
These tests establish process-crash behavior on tested filesystems, not an
unqualified guarantee against every storage device's power-loss behavior.
