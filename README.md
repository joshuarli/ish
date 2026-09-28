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
| Ctrl+R | Fuzzy search (subsequence, case-insensitive) |

Fuzzy search opens a pager with matching characters highlighted in yellow. Up/Down to navigate, Enter to accept, Escape to cancel.

**Scored ranking**: results are ranked by match quality, not just recency. Entries recorded in the current directory or one of its ancestors get a priority boost. Contiguous matches (searching "target" finds literal `target/release/` first), word-boundary alignment (`deb` prefers `debug/` over scattered d-e-b), and optimal alignment via forward+backward scan find the tightest match window — "test" in "the best test" finds the contiguous "test" at the end, not scattered letters.

Stored at `~/.local/share/ish/history`. Deduplicated on add.

`history reset` deletes all saved history and invalidates cached history in
other running ish shells.

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
undo run --scope . -- make install   # record what an arbitrary program changes
```

**What is captured.** One accepted input is one transaction. Within it, ish
captures:

- native `rm` and `mv`, resolved after expansion (aliases, globs, quoting,
  and `command rm` all reach them; an explicit path such as `/bin/rm` runs
  the external utility without capture);
- writable redirections (`>`, `>|`, `>>`, `<>`, `&>`), including through
  symlinks, in pipeline stages, and in command substitutions.

Every other external command runs normally and is **not** captured; `undo
list` and `undo show` report such inputs as having uncaptured commands.
`undo run --scope <dir> -- <program> [args...]` covers arbitrary programs:
it checkpoints the directory tree before the program starts and compares it
after the program (and the children it waits for) completes. That records
observed changes within one tree over one interval. It is not a sandbox,
process attribution, or an atomic snapshot: concurrent writers and detached
descendants that keep running can make recovery conflict or stay uncertain.
Arguments expanded before the program starts, including command
substitutions, are outside the checkpoint.

**Native commands.** `rm` supports `-r`/`-R`, `-f`, `-i`, `-v`, and `--`;
`mv` supports `-f`, `-i`, `-n`, `-v`, and `--`. Other options fail with
status 2 instead of silently running an unprotected utility. Recursive
removal does not cross mount points and never follows symlinks. The
filesystem root, the undo store, and the store's ancestors are refused. A
same-filesystem `mv` is a rename and copies nothing; its undo moves the same
object back, carrying later edits along. A cross-filesystem `mv` copies to a
staging name beside the target, publishes it, and only then removes the
source. `rm -f` ignores missing files but never skips preservation: if a
version cannot be preserved, the command fails and nothing is removed.

**Preservation.** Before anything is removed or overwritten, the affected
version is saved and recorded durably:

| Kind | When | Contract |
|---|---|---|
| clone | the store is on the same filesystem and it supports cloning (APFS, btrfs, XFS) | frozen; no data moves |
| linked | cloning is unavailable and the name is being unlinked | the retained inode; another hard link or an open writer can still change it |
| copy | cloning is unavailable and contents must stay frozen (redirections, scoped checkpoints, other filesystems) | frozen, bounded by `ISH_UNDO_COPY_LIMIT` and `ISH_UNDO_MIN_FREE` |

A write that needs a frozen pre-image and cannot get one is refused. `undo
show` labels linked versions and every fidelity limitation (for example,
ownership that could not be restored without privileges).

**Replay.** `undo` checks the current filesystem before every step and never
discards newer data: a deleted file is restored only where nothing exists
now, a created or modified file is reverted only while it still matches what
the transaction left (compared against a frozen post-image where one was
taken), and a move is reversed only while the moved object is still there.
Anything else is a conflict that is reported and left alone. `--force` first
saves the conflicting current state, then proceeds; `undo redo` can bring it
back. Runs are restartable: an interrupted or partly conflicting run is
simply run again and skips completed steps.

```
undo [id] [--dry-run] [--only <path>] [--force]
undo redo [id] [--dry-run] [--only <path>] [--force]
undo list [-a]          # recent transactions; * marks this shell's
undo show [id]          # coverage, state, stored versions, changes
undo diff [id]          # bounded text diffs; binary and large files summarized
undo run --scope <directory> -- <program> [arguments...]
undo gc                 # apply the retention limits now
undo purge <id>         # delete one transaction's saved versions
undo doctor             # store health and capability probes
undo volume add <dir> | list | remove <dir|id>
```

Plain `undo` and `undo redo` act on this shell's latest eligible
transaction; ids address any shell's. `--only` paths are resolved against
the directory the transaction ran in, and restoring a path also restores the
directories and moves it depends on. `undo list`, `show`, `diff`, and
`doctor` work in pipelines and command substitutions; commands that change
recovery state must run in the shell itself. A job stopped with Ctrl+Z keeps
its transaction open until it finishes after `fg`.

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
| `ISH_UNDO_SCOPE_LIMIT` | `250000` | entries one scoped checkpoint may catalog |

Collection runs at most hourly after a command that recorded something, or
on `undo gc`, oldest first. It never collects active or interrupted
transactions, those with versions on a disconnected volume, or the newest
transaction. Sizes are shown as logical bytes, allocated blocks (an
overestimate for clones, whose blocks may be shared), and bytes copied;
exclusive copy-on-write usage is not measured.

**Durability.** Saved versions and the journal record describing them are
flushed with `fsync` before the change they protect is made, in batches
during recursive removal. A crashed or killed shell leaves a transaction
that `undo list` shows as interrupted and that `undo` recovers from: records
after a torn write are ignored, operations whose outcome was not recorded
are checked against the filesystem, and saved versions are never deleted
automatically. On macOS, `fsync` does not flush the drive's write cache
(`F_FULLFSYNC` would), so after a power loss the most recent records may be
missing; filesystems mounted without ordering guarantees can lose recent
records on Linux too.

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

A Rust workspace: the `ish` package (library target and interactive binary) and `crates/ish-undo`, which owns native `rm`/`mv`, protected redirection opens, scoped checkpoints, the recovery store and journals, and replay. Platform code primarily uses `rustix`; `libc` remains a narrow compatibility escape hatch.

Shell-owned operations stay native where possible: directory listing, git detection, pathname expansion, and completion do not spawn subprocesses. Running external commands, command substitutions, and the denv integration may spawn processes.

Signal handling uses the self-pipe pattern. Each pipeline gets its own process group. Terminal foreground control via `tcsetpgrp`. Raw mode via `termios`.
