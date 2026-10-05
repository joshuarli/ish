# ish

A purely interactive shell.

No scripting, no POSIX compat, no plugins.


## Features

### Prompt

```
josh@mac ~/d/ish* main $
```

- User, host, abbreviated working directory, git branch, exit status color
- PWD shortens middle components: `~/.config/fish` becomes `~/.c/fish`
- Green on success, red on failure
- Git branch from `.git/HEAD` (cached, no subprocess). Detached HEAD shows short hash
- Red `*` when `__DENV_DIRTY=1`

### Line Editing

| Key | Action |
|---|---|
| Ctrl+A / Home | Beginning of line |
| Ctrl+E / End | End of line |
| Ctrl+K | Kill to end of line |
| Ctrl+U | Kill to start of line |
| Ctrl+W / Ctrl+Backspace | Kill word backward |
| Ctrl+Y | Yank (paste) killed text |
| Ctrl+D | Delete forward / exit on empty line |
| Ctrl+C | Cancel line |
| Ctrl+L | Clear screen |
| Ctrl+P (any mode) | Write a layout dump to `~/.cache/ish/dump-<random hex>` without changing the screen |
| Left / Right | Move by character |
| Ctrl+Left / Alt+B | Move word left |
| Ctrl+Right / Alt+F | Move word right |

Single kill ring shared across Ctrl+K/U/W. Full UTF-8 support.

**Multiline editing**: paste a multi-line command (with `\` continuations or unclosed quotes) and move freely with Up/Down between lines. Long single-line input also behaves like a wrapped grid: Up/Down move by visual row and preserve the target column. At the first or last input row, Up/Down resume history navigation.

### History

| Key | Action |
|---|---|
| Up / Down | Move by visual row in wrapped input; prefix search through history at the boundary |
| Ctrl+R | Search saved commands across all sessions (case-insensitive) |

Fuzzy search opens a pager with matching characters highlighted in yellow. Up/Down to navigate, Enter to accept, Escape to cancel.

**Ranking**: prefix matches come first, followed by word-boundary substrings, other substrings, and scattered-letter matches. Within the same match quality, commands used in the current directory or nearby ancestors are preferred; usage frequency and timestamps refine the order. A weak local match cannot outrank a stronger literal match elsewhere. An empty search shows recent commands first. Matching characters are highlighted, including Unicode text.

Up/Down and autosuggestions use the history present when the session started,
plus commands entered in that session. Ctrl+R also sees commands saved by other
running sessions when search opens.

Commands are saved before execution in `~/.local/share/ish/history.sqlite3`.
Every occurrence is retained for ranking; the history list and search show
each command once. Writes are transactional and storage errors are reported.
An interrupted command or abruptly killed shell does not need a clean exit
to save its accepted input. The bare `l` command is excluded from history.

The first start imports the old `history` text file and `history.bin` cache
without modifying them. Import happens once. Restart old ish instances after
upgrading: they still write to the old files, and those later writes are not
imported into the new database.
The legacy `scripts/import-fish-history` helper must run before this migration;
it refuses to write once the database exists.

`history reset` clears the database and invalidates history in other running
ish shells. Preserved legacy import files remain untouched. `history compact`
reclaims database storage without losing
commands from other sessions. The old cache-specific `history rebuild`
subcommand has been removed. Storage subcommands require a standalone command;
plain `history` can be redirected or used in a pipeline without changing storage.

Run `history -h` for the available history storage commands.

### Tab Completion

| Context | Behavior |
|---|---|
| Empty line | Inserts `cd ` |
| After `cd ` | Directories only |
| `$` prefix | Environment variables |
| Everything else | Files and directories |

Completions display in a column-major grid (up to 6 columns, 10 visible rows). Navigate with arrow keys or Tab, accept with Enter, cancel with Escape. Typing filters live. Path completions sorted by modification time (most recent first) so recently-built or recently-edited files appear at the top.

Colors: blue for directories, cyan for symlinks, green for executables.

### Syntax

Pipelines, command lists, and redirections:
```
ls -la | grep foo > out.txt && echo done
cat err.log |& head          # pipe stderr too
make 2> err.log              # redirect stderr
make &> all.log              # redirect both
cmd1 || cmd2 ; cmd3
```

Quoting:
```
echo 'literal $HOME'         # single quotes: no expansion
echo "hello $USER"           # double quotes: variables expand
echo it\'s\ a\ test          # backslash escaping
```

Continuation: unclosed quotes, trailing `|`, `&&`, `||` prompt for more input.

Comments with `#`.

### Expansion

```
~/file             # tilde → $HOME
$PATH              # environment variables
$(whoami)          # command substitution
`date`             # backtick substitution
*.rs               # glob: any characters
test?              # glob: single character
src/**/*.py        # glob: recursive descent
```

Expansion order: tilde, parameter expansion, command substitution, pathname expansion (globbing). Quoted characters skip expansion. No match on glob is an error.

### Builtins

| Command | Description |
|---|---|
| `cd [dir]` | Change directory. `cd -` for previous |
| `exit [code]` | Exit (warns if job suspended) |
| `fg` | Resume suspended job |
| `export [NAME[=value]]` | Export a variable to child processes. No args lists all |
| `set [VAR [val]]` | Set env var. No args lists all |
| `unset VAR...` | Remove env vars |
| `alias [name [cmd]]` | Define/list aliases |
| `l [path]` | Native directory listing |
| `c` | Clear screen |
| `w` / `which` / `type` | Locate command |
| `echo [args]` | Print arguments |
| `pwd` | Print working directory |
| `true` / `false` | Return 0 / 1 |
| `copy-scrollback` | Copy session to clipboard via OSC 52 |
| `ish-dump` | Write the current layout state to `~/.cache/ish/dump-<random hex>` (same as Ctrl+P) |
| `rm [-f \| -i] [-rRv] [--] file...` | Protected native removal (see [Undo](#undo)) |
| `mv [-f \| -i \| -n] [-v] [--] source... target` | Protected native move |
| `undo ...` | Revert, reapply, inspect, and manage recorded changes |

### `l` — Native Directory Listing

Equivalent to `ls -plAhG`, implemented without forking:

```
drwxr-xr-x  12 josh  staff   384B  Mar 17 10:23  src/
-rw-r--r--   1 josh  staff   1.2K  Mar 17 09:15  Cargo.toml
lrwxr-xr-x   1 josh  staff    11B  Mar 10 14:02  link -> target/debug
-rwxr-xr-x   1 josh  staff   184K  Mar 17 17:54  ish
```

Human-readable sizes. Colors: blue dirs, cyan symlinks, green executables, red setuid. Sorted case-insensitively. Resolves owner/group names via `getpwuid`/`getgrgid`.

### Aliases

Define at the prompt or in config:
```
alias ll l
alias gs git status
```

Aliases expand inline when you press space. `w`/`which`/`type` check aliases first.

### Job Control

Ctrl+Z suspends the foreground job. `fg` resumes it. One job slot — simple and intentional. Shell warns before exiting with a suspended job (exit again to force).

If an AND-OR list was suspended while running its first command (`a && b`, suspended during `a`), `fg` resumes and continues the list.

### denv Integration

Automatic `.envrc`/`.env` loading when [denv](https://github.com/joshuarli/denv) is in PATH. Runs on every `cd` with a fast-path check (file mtimes vs sentinel) to skip the subprocess when nothing changed. `denv allow`, `denv deny`, `denv reload` work as expected.

### Undo

ish records what its own filesystem operations change, so they can be
reverted and reapplied:

```
rm -r build            # removed, but recoverable
undo                   # restores build/
undo redo              # removes it again
echo notes > log.txt   # truncation of an existing log.txt is recoverable too
```

**What is captured.** One accepted input is one transaction. Within it, ish
captures:

- native `rm` and `mv`, resolved after expansion (aliases, globs, quoting,
  and `command rm` all reach them; an explicit path such as `/bin/rm` runs
  the external utility without capture);
- writable redirections (`>`, `>|`, `>>`, `<>`, `&>`), including through
  symlinks, in pipeline stages, and in command substitutions.

Every other external command runs normally and is **not** captured; `undo
list` and `undo show` report such inputs as having uncaptured commands. What
a program does to files internally is outside what ish can record.

**Native commands.** `rm` supports `-r`/`-R`, `-f`, `-i`, `-v`, and `--`;
`mv` supports `-f`, `-i`, `-n`, `-v`, and `--`. Other options fail with
status 2 instead of silently running an unprotected utility. Recursive
removal does not cross mount points and never follows symlinks. The
filesystem root, the undo store, and the store's ancestors are refused.
`mv` is a rename and copies nothing; its undo moves the same object back,
carrying later edits along. A `mv` between filesystems is a copy and a
removal, which native `mv` does not do: it is refused, nothing is moved, and
the message points at `/bin/mv` for an unprotected move. `rm -f` ignores
missing files but never skips preservation: if a version cannot be
preserved, the command fails and nothing is removed.

**Preservation.** Before anything is removed or overwritten, the affected
version is saved and recorded durably:

| Kind | When | Contract |
|---|---|---|
| clone | the store is on the same filesystem and it supports cloning (APFS, btrfs, XFS) | frozen; no data moves |
| linked | cloning is unavailable and the name is being unlinked | the retained inode; another hard link or an open writer can still change it |
| copy | cloning is unavailable and contents must stay frozen (redirections, files on filesystems the store cannot clone from) | frozen, bounded by `ISH_UNDO_COPY_LIMIT` and `ISH_UNDO_MIN_FREE` |

A write that needs a frozen pre-image and cannot get one is refused. `undo
show` labels linked versions and every fidelity limitation (for example,
ownership that could not be restored without privileges).

**Replay.** `undo` checks the current filesystem before every step and never
discards newer data: a deleted file is restored only where nothing exists
now, a created or modified file is reverted only while it still matches what
the transaction left (compared against a frozen post-image where one was
taken), and a move is reversed only while the moved object is still there.
Anything else is a conflict: it is reported and left untouched, everything
that can be undone still is, and there is no option to override a conflict.
Resolve it (move your newer file aside, or delete it) and run `undo <id>`
again; completed steps are skipped, so a retry only does what remains. The
same holds for `undo redo`. `--dry-run` lists the steps a run would perform
and touches nothing.

```
undo [id] [--dry-run]
undo redo [id] [--dry-run]
undo list [-a]          # recent transactions; * marks this shell's
undo show [id]          # coverage, state, stored versions, changes
undo gc                 # apply the retention limits now
undo purge <id>         # delete one transaction's saved versions
undo doctor             # store health and capability probes
undo volume add <dir> | list | remove <dir|id>
```

Plain `undo` and `undo redo` act on this shell's latest eligible
transaction; ids address any shell's. `undo list`, `show`, and `doctor` work
in pipelines and command substitutions; commands that change recovery state
must run in the shell itself. A job stopped with Ctrl+Z keeps its
transaction open until it finishes after `fg`. While a shell is undoing or
redoing a transaction (state `replaying`), no other command touches it:
`undo`, `undo redo`, `gc`, and `purge` all leave it alone.

**Storage and retention.** Transactions and saved versions live under
`~/.local/share/ish/undo` (private, versioned). Clones and hard links only
work within one filesystem, so for another volume, register a private store
with `undo volume add <mount point>`; its versions stay there, and while the
volume is disconnected its transactions are reported as unavailable, never
deleted. Retention is set with `set` in `config.ish` or at the prompt:

| Variable | Default | Meaning |
|---|---|---|
| `ISH_UNDO` | `on` | `off` runs `rm`/`mv` as external commands and opens redirections unprotected |
| `ISH_UNDO_COPY_LIMIT` | `64M` | bytes one operation may copy when cloning is unavailable |
| `ISH_UNDO_MAX_SIZE` | `8G` | logical size of retained versions |
| `ISH_UNDO_MAX_ENTRIES` | `500` | retained transactions |
| `ISH_UNDO_MAX_DAYS` | `30` | age limit |
| `ISH_UNDO_MIN_FREE` | `1G` | free space to leave when copying |

Collection runs at most hourly after a command that recorded something, or
on `undo gc`, oldest first. It never collects active or interrupted
transactions, those with versions on a disconnected volume, those being
replayed, or the newest completed transaction. Automatic collection is
bounded by the work it does, not by how much it deletes: a pass loads at
most 32 journals (and removes at most 32 orphaned volume directories), totals
sizes from a small file written when each transaction is sealed, and picks up
where it stopped at the next command boundary; `undo gc` has no such bound. Sizes are shown as logical bytes,
allocated blocks (an overestimate for clones, whose blocks may be shared),
and bytes copied; exclusive copy-on-write usage is not measured.

**Durability.** Saved versions and the journal record describing them are
flushed with `fsync` before the change they protect is made, in batches
during recursive removal. Undo and redo follow the same order: before a step
removes or replaces anything, the version it displaces is saved, and a
prepare record naming it and the hidden staging entry the step created beside
its destination (`.ish-undo-<id>-<run>-<step>-<random>`, created exclusively
and never reused) is flushed; only then does the step change the filesystem,
and only afterwards is it recorded as done. A crashed or killed shell leaves
a transaction that `undo list` shows as interrupted, or a replay that simply
stopped, and the next run reconciles it: records after a torn write are
ignored, operations whose outcome was not recorded are checked against the
filesystem, a recorded staging entry is removed only while it is still the
object that was recorded, and a step whose change already happened takes its
displaced version from the prepare record, so redo still has it. Saved
versions are never deleted automatically. A crash can leave a staging entry
behind until the transaction is retried. On macOS, `fsync` does not flush
the drive's write cache (`F_FULLFSYNC` would), so after a power loss the
most recent records may be missing; filesystems mounted without ordering
guarantees can lose recent records on Linux too.

Directory and file metadata (modes, timestamps, extended attributes, ACLs
and resource forks on macOS, BSD flags) is restored where an unprivileged
user can set it; inode numbers and change times are not. Directory ACLs on
macOS are not recorded. Files with hard links outside a removed set come
back as independent files, and the other links keep their newer contents.

### Config

`~/.config/ish/config.ish`:

```
# Environment
set EDITOR nvim
set PAGER less

# Aliases
alias ll l
alias gs git status
alias .. "cd .."
```

Two directives: `set` and `alias`. Variables expand in values. Comments with `#`.

## Non-Features (by Design)

Every omission is deliberate. No scripting engine means no code injection, no `source`-based exploits, no eval chains. No `if`/`for`/`while`/functions means no control flow to hijack. No `${VAR}` brace expansion means no expansion-based attacks. One suspended job (no `&` backgrounding) means no resource exhaustion through job spawning. No plugins means no supply chain.

The result: the shell has a small, auditable interactive command evaluator. If you can't `source` it, you can't trick a user into `source`-ing it.

- No scripting. `ish script.sh` prints an error.
- No POSIX compliance.
- No `source`, no `eval`, no `${VAR}` brace expansion.
- No `if`/`for`/`while`/functions.
- No plugins, no prompt customization, no themes.
- No background jobs (`&`). One suspended job only.

## Architecture

A Rust workspace: the `ish` package (library target and interactive binary) and `crates/ish-undo`, which owns native `rm`/`mv`, protected redirection opens, the recovery store and journals, and replay. Platform code primarily uses `rustix`; `libc` remains a narrow compatibility escape hatch.

Shell-owned operations stay native where possible: directory listing, git detection, pathname expansion, and completion do not spawn subprocesses. Running external commands, command substitutions, and the denv integration may spawn processes.

Signal handling uses the self-pipe pattern. Each pipeline gets its own process group. Terminal foreground control via `tcsetpgrp`. Raw mode via `termios`.
