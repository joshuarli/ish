# Fuzzy pickers

`fz` reads candidate bytes from stdin and writes selected bytes to stdout.
Picker interaction uses `/dev/tty`, or the terminal selected by `--tty`.
The builtin ships inside ish. An installed fz executable is unnecessary.

```sh
printf 'alpha\nbeta\n' | fz
fz --multi --query=worker < candidates.txt > selected.txt
printf 'alpha\nbeta\n' | fz --show-matches=alp
printf '<%s>\n' "$(printf 'alpha\nbeta\n' | fz)"
```

`fz --help` lists the shared standalone options. The builtin defaults to one
search worker. `--workers` can explicitly enable scoped parallel searches. `--show-matches` ranks and
prints without opening a terminal. `--read-null` changes input framing.
Output remains newline framed. Duplicate records retain separate identities.
ASCII lowercase query bytes match either case. Uppercase bytes match uppercase
only. Non-ASCII bytes match exactly. Candidates longer than 1024 bytes remain
eligible without fzy scores or highlights.

Default Enter accepts the selected row, or the query when nothing matches.
Multi-select never outputs an unmatched query. Down enters result focus.
Space toggles a mark in results and inserts a space while editing the query.
Up from the first row returns to the query. Marks survive filtering, and Enter
outputs marks in marking order. Without marks, Enter accepts the focused row.
Escape and Ctrl+C cancel without selected stdout. A sole match still requires
Enter. A terminal too small for multi-select returns an error.

Filtering returns 0 even with no matches. Interactive cancellation and
multi-select no-match return 1. Terminal and I/O errors return 1 in default mode
and 2 in multi-select mode. Invalid CLI arguments follow standalone fz's status
1 contract. Handled termination signals restore the terminal before signal death.
The standalone key bindings ignore typed Ctrl+Z. Externally stopped workers
retain ish's normal foreground-job ownership and resume through `fg`.

## Process selection

```sh
pk worker
pk --port 8080
pk -9 --port 8080 worker
pk -- -option-looking-query
```

`pk` requires a statically named whole input line. Options precede the first
query operand. Query operands join with spaces. Ports are decimal integers
from 1 through 65535, including leading zeros. Bare `pk` is a usage error.
`-h` and `--help` show usage.

The picker takes an immutable process snapshot. Rows show the PID, executable basename,
and arguments with control characters and invalid UTF-8 escaped. Selection
uses original records rather than row text. Down, Space, Up, and Enter follow
fz's multi-select behavior. Its prompt is `TERM> ` or `KILL> `, and the info row
shows the marked count. Explicit acceptance authorizes the selected signals.
Cancellation, EOF, terminal errors, and unmatched queries authorize none. Process selection
requires at least 20 columns and four rows.

Candidates have the invoking shell's numeric effective UID. The real UID is
also retained. PID 1, ish, and every resolved invoking ancestor PID are
excluded. Ordinary ish children remain eligible, including suspended jobs.
Incomplete ancestry resolution aborts lookup. Up to 4096 candidate handles
remain open for a session. Exceeding that bound or exhausting descriptors
fails lookup rather than truncating the snapshot.

Every selected ID must belong to the snapshot. Duplicate IDs, duplicate PIDs,
invalid lifetimes, and protected processes reject the whole batch before any
signal. Each target is then checked for lifetime, both UIDs, parent PID,
executable and argv changes. Port selections also require a current TCP
listener. Exited, changed, inaccessible, and ineligible targets are skipped
and reported. Remaining valid targets still receive exactly TERM or KILL.
No process group, tree, or automatic escalation is involved. Reports say
“signal sent” because a successful send does not prove exit.

TCP lookup includes IPv4 and IPv6 LISTEN sockets and joins actual descriptor
ownership. Shared sockets and multiple holders remain distinct process records.
UDP and established connections do not qualify. Linux reads the shell network
namespace's TCP tables and joins socket inodes to process descriptors. macOS
uses libproc's per-process socket information and rechecks socket identity.
Visibility warnings describe incomplete inspection without attributing a port
to another user.

Linux requires pidfd support, available since kernel 5.3. A pidfd is acquired
while collecting each displayed record and checked against coherent metadata.
The handle binds the final send to that lifetime, preventing PID reuse from
redirecting the signal. It does not freeze executable, arguments, ownership,
parentage, or sockets. Those facts are separately rechecked. Ordinary pidfd
errors never fall back to numeric PID signaling.

macOS retains libproc start timestamps and rechecks native metadata before
`kill`. This compatibility path has a check-to-signal race, including possible
PID reuse. It is not identity-bound signaling. The SDK declares
`proc_signal_with_audittoken`, but does not supply a supported token-construction
contract for arbitrary listed processes. This implementation does not infer
availability or token validity from XNU source. It never uses `proc_terminate`,
which can choose a signal different from the requested TERM or KILL.

`pk` statuses are:

| Status | Meaning |
|---|---|
| 0 | Help, cancellation, no selection, no eligible processes without a port filter, or every selected signal sent |
| 1 | Lookup failure, no eligible listener for a requested port, invalid batch, or a skipped or failed selected target |
| 2 | Usage or picker terminal/I/O failure |

HUP and TERM stop selection or further batch sends and propagate through ish's
termination lifecycle. Already completed signals are reported and remain sent.
`fg` checks and reaps a completed owned job before attempting to resume its group.

## Shared source

[fz](https://github.com/laputa-systems/fz) is the canonical implementation.
[UPSTREAM](../vendor/fz/UPSTREAM) records the exact vendored revision and license.
The runtime dependency is a local package with a committed lockfile. Normal
builds fetch no fz repository and require no fz executable.

`Picker` owns editing, focus, stable candidate IDs, marking, and rendering.
The host supplies events and owns the terminal. `pk` supplies ish's input and
signals and restores the terminal before signaling. `fz` runs the shared CLI
in a fresh same-binary worker. Epsh's owned worker registration resolves it
independently of PATH and preserves argv bytes, environment, cwd, descriptors,
and foreground ownership. No inherited host callback runs the picker.
Explicit executable paths retain ordinary external behavior.

Ctrl+R uses the same byte scanning and subsequence eligibility primitives on
borrowed history text. Its Unicode matching, character highlights, literal
ranking tiers, context, 200-row display bound, and refinement cache remain in
ish. It does not invoke fzy scoring or inherit its 1024-byte boundary.

Update from a reviewed, committed fz checkout:

```sh
scripts/vendor-fz.sh /path/to/fz COMMIT
cargo check --workspace
cargo test --test pty fuzzy_builtin
cargo test --test pty process_picker
```

The updater copies upstream source without local matcher or picker patches.
It retains `Cargo.toml.orig` and normalizes the local manifest with jaq:
standalone profiles, workspace settings, benchmarks, and development dependencies
are removed. Automatic binary and test discovery are disabled for the vendored
library. Run fz's full CLI, PTY, and differential tests in the canonical checkout
before updating. Commit the upstream change before recording its revision here.
