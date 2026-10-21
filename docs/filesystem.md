# File operation reference

ish provides in-process `cp` and `mv` commands for interactive use. They accept
raw Unix path bytes and take precedence over bare `cp` and `mv` entries on
`PATH`. Use an explicit executable path, such as `/bin/cp`, when you want the
system utility.

## Choose a destination form

Use one of these forms:

```text
cp [OPTIONS] SOURCE DESTINATION
mv [OPTIONS] SOURCE DESTINATION
cp [OPTIONS] --into DIRECTORY SOURCE...
mv [OPTIONS] --into DIRECTORY SOURCE...
```

The two-operand form maps the source object to exactly `DESTINATION`. It never
adds the source basename because the destination happens to be a directory.
Thus `cp report.txt .` reports a file-to-directory type conflict. To put the
file inside the current directory, use `cp --into . report.txt`.

`--into DIRECTORY` requires an existing directory and one or more sources. Each
source maps to a child named after its basename. The directory may resolve
through a final symlink because this form explicitly selects a container. A
source with no unambiguous basename, such as `/`, `.` or `..`, is rejected.

Directories recurse automatically. `cp -r` and `cp -R` are accepted but have no
effect. `mv` rejects those flags. A destination parent must already exist.
Trailing slashes do not change operand mapping or make a final symlink writable
through its target. A final source symlink is copied or moved as a symlink.

Use `--` before operands that begin with `-`. ish preserves filename bytes,
including spaces, newlines, and non-UTF-8 bytes. It escapes non-printable bytes
in diagnostics.

## Select collision behavior

With no collision option, an existing destination is an error. ish does not
prompt or overwrite it. The options below select other behavior:

| Option | Behavior |
|---|---|
| `-n`, `--no-clobber` | Skip existing targets. A skipped move source stays in place. |
| `--replace` | Replace a regular file with a regular file or a symlink with a symlink. |
| `--merge` | Recurse into an existing directory and preserve destination-only entries. |

`--replace` and `--no-clobber` cannot be combined. `--merge` can be combined
with either. Without `--merge`, an existing directory is a collision, even when
empty. `--replace` never replaces a directory tree. A file/directory or
file/symlink type conflict is an error. With `-n`, any existing target may be
skipped without changing it. A destination symlink is an object, not a path to
its referent.

During a merge, existing destination directories retain their configured
ownership, mode, ACLs, and extended attributes. Child operations can change
their timestamps. Newly created directories receive the source's supported
metadata after their children are complete.

The identical source and destination directory entry is a successful no-op.
Different hardlink names for one inode are separate entries and follow the
collision option. Replacing one destination hardlink name does not change its
other aliases.

ish checks all top-level mappings before mutation. It rejects duplicate output
names, overlapping source operands, and source/destination ancestry conflicts.
It does not predict case-folding or Unicode-normalization collisions from path
text; the filesystem decides those during publication.

## Use options and statuses

`cp` and `mv` accept these options:

| Option | Meaning |
|---|---|
| `--into DIR` | Map each source to a child of an existing directory. |
| `--merge` | Merge directory contents and retain destination-only entries. |
| `--replace` | Replace supported leaf targets. |
| `-n`, `--no-clobber` | Skip existing targets. |
| `--dry-run` | Report planned mappings and actions without creating destination objects or staging records. |
| `--durable` | Synchronize copied data and publication before successful copy completion. |
| `--cross-file-systems` | Permit recursive traversal across nested mount points. |
| `-j N`, `--jobs N` | Request 1 to 32 transfer workers, subject to available resources. |
| `-v`, `--verbose` | Report completed operations. |
| `--no-progress` | Suppress terminal progress output. |
| `-h`, `--help` | Show command help. |
| `--` | End option parsing. |

Without `--durable`, `cp` does not promise synchronized storage. Cross-filesystem
`mv` always requires durable destination publication before source retirement.
`--dry-run` reports current observations and planned actions; the filesystem can
change before a later real run. A dry run does not probe clone support by
creating a file. Reads can still update access times or filesystem bookkeeping.

Status 0 means the selected policy completed, including intentional skips and
same-entry no-ops. Status 1 means an operational failure or partial completion.
Status 2 means invalid syntax or an unsupported execution context. Status 130
means user cancellation. When a command skips entries, it reports a summary to
stderr even without `--verbose`.

## Understand copy and move completion

Copy preserves logical file contents and length, directory structure, symlink
target bytes, and representable access and modification timestamps. It also
preserves applicable ACLs and extended attributes, resource forks where
supported, supported platform flags, and hardlink relationships among
transferred source names. New objects use normal ownership for the invoking
user. ish preserves ordinary permission bits subject to safe normalization of
ownership-dependent privilege bits and capabilities.
If required ACL or security metadata cannot be represented safely, ish reports
the limitation instead of silently broadening access. Unsupported required
metadata or flags fail the affected transfer and retain its source; a merge may
already have published other completed entries.

An explicit file replacement publishes a completed new object atomically at
the selected name. It does not truncate the previous destination inode, so
other hardlinks to that inode remain unchanged. ish does not require a content
hash to copy a file. Clone, native-copy, and buffered paths follow the same
logical metadata policy.

Sparse regions are preserved where the destination supports them. Physical
extent layout may differ. A destination without sparse support may require
dense storage. ish warns when the source reports allocation below its logical
length and the destination reports dense allocation. The warning reports block
accounting; filesystem sparse support and exact hole layout remain unverified.
Capacity failures do not authorize deleting source data.

An isolated same-filesystem non-merge `mv` uses a rename without walking
descendants when the filesystem permits it. Multiple operands and merges can
couple renamed directories to later hardlink aliases. ish indexes completed
renames when a later alias needs them. When an earlier copied group could
split, ish inspects the next directory and transfers a coupled tree instead
of renaming it. Regular files and hardlinked symlinks keep their group topology.

A cross-filesystem move copies transferable
objects and applies required metadata. It synchronizes destination data and
directory publication before revalidating and retiring transferred source objects.
Intentional skips, unsupported objects, changed entries, and failed transfers
remain at the source. Move completion is per operand. It is not a transaction
across all operands or a whole merge.

An operation can publish a destination and retain its source if later
synchronization or source checks fail. Source retirement can also finish
partially. Diagnostics for reported partial outcomes identify the source and
destination paths or retained source entries. Inspect both trees before
retrying or removing either one. ish does not roll back completed destination
entries to make a partial move look atomic.

Cross-filesystem moves preserve source ownership when the destination permits
it. If a move cannot reproduce meaningful ownership, ACL, extended attribute,
resource fork, timestamp, permission, or flag semantics, ish reports a failure
and retains the source. Hardlinks outside the transferred source set cannot
stay linked across filesystems. Filesystem timestamp precision may round
values.

Move receipts retain the version observed after a successful transfer and
required metadata application. Publication and merge aggregation validate that
version. They do not accept a later version merely because the inode matches.
An observed content or metadata change prevents retirement of the affected
source entries.

Neither operation preserves ctime, inode numbers, or identical physical extent
allocation. Linux cannot set creation time, so creation time is outside the
Linux preservation contract. macOS attempts to preserve birth time and
platform flags. A meaningful flag or required metadata field that the
destination cannot represent fails the affected transfer and retains its
source. Platform-irrelevant fields are outside the contract.

macOS resource forks up to 4 GiB use bounded descriptor-based copying with
cancellation checks between chunks. Larger forks require a named stream whose
identity can be verified against the held file descriptor. An unverifiable
stream fails with an unsupported-operation diagnostic and retains the source.
The larger-fork path still requires native validation.

`cp` and cross-filesystem `mv` transfer regular files, directories, and symlink
objects. An unsupported special object fails without opening it as a byte
stream. A same-filesystem `mv` can rename such an object without reading it.

If names in one source hardlink group would land on different destination
filesystems, ish cannot preserve that relationship. It reports the conflict
and retains source names not yet retired. Earlier completed names remain
reported as partial work. ish does not replace a hardlink group with separate
copies.

`mv` refuses filesystem roots and mount roots, the home directory, the current
directory, and its ancestors. A same-filesystem rename does not make these
roots safe to move.

## Understand mounts and concurrent changes

Recursive traversal stops at nested mount points by default and reports an
operational error. It does not treat an empty-looking mount point as an empty
directory. `--cross-file-systems` explicitly permits traversal into nested
mounts; it does not unmount them. A mount point that cannot be retired remains
and is reported as partial completion. Source and destination roots may be on
different filesystems.

ish anchors paths at open directory descriptors, stages new objects on the
destination filesystem, and rechecks observed identities and versions. Stable
source and destination trees are the full-correctness baseline. These checks do
not provide an atomic conditional rename or unlink against every concurrent
change. ish detects observed changes and retains ambiguous objects. It does not
protect against a same-account process with authority to modify the same trees.

## Inspect retained operation state

Normal error and partial-completion diagnostics name retained staging or source
paths when the engine can identify them. If ish or the machine stops abruptly,
there may be no diagnostic. Inspect the destination parent for
`.ish-fs-stage-*` names and the source parent for `.ish-fs-retired-*` and
`.ish-fs-intent-*` names before retrying. Identify each entry before removing
or moving it. ish does not promise cleanup after forced termination, sweep
retained names, or provide an automatic recovery command. Receipt spools use
unlinked files under the configured scratch directory and disappear when the
operation closes them.

## Run native file commands in ish

`cp` and `mv` run in the original shell process. You can use them in lists and
with redirections:

```sh
cp source destination && echo copied
cp source destination >output.log 2>errors.log
mv first second; cp "$(printf '%s' source)" destination
```

Standalone negation also runs the operation in the original process. A
successful `! cp source destination` copies the object and returns status 1.
A failed `! mv missing destination` returns status 0. Negation leaves
cancellation at status 130. A refused target still produces its diagnostic
and has no target effects, even when enclosing negation changes the status.

`command -v`, `command -V`, and `type` identify bare native command names as
shell builtins, including with an empty `PATH`. Explicit paths retain external
classification.

Argument substitutions finish before the file operation starts. ish refuses a
native `cp` or `mv` target in a pipeline or command substitution with status 2,
before opening that target's redirections or invoking its handler. This applies
to aliases, expanded command names, and nested `command` dispatch. An explicit
program path such as `/bin/cp` remains an external command and follows external
job control.

Ctrl+C requests cooperative cancellation and returns status 130 after active
workers reach safe boundaries and finish cleanup. A kernel operation may delay
that response. ish ignores SIGTSTP during a native file operation, so Ctrl+Z
does not suspend it. External foreground jobs retain their normal Ctrl+Z and
`fg` behavior. Signal handling, terminal settings, progress output, and workers
are restored or joined before the next prompt.

Progress appears on stderr's terminal after 200 ms. `--no-progress` suppresses
it. Normal quick operations stay quiet; `--verbose` reports completed
operations, and `--dry-run` reports planned actions. Progress reports bytes
streamed and logical bytes cloned. These count different transfer methods; do
not add them to estimate physical I/O. Logical clone size does not report
physical I/O, and ish makes no universal speed-ratio claim.
