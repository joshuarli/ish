# ish

ish is an interactive shell for macOS and Linux. It provides a native prompt,
line editing, history search, completion, aliases, minimal job control, native
`cp` and `mv` commands, and protected deletion. It has no script-file or
command-string mode.

## Install and run

Download a pre-release archive for your platform from
[GitHub Releases](https://github.com/joshuarli/ish/releases). The release
workflow provides macOS Apple silicon and Linux x86_64 and ARM64 builds; Linux
has static and dynamic variants. Extract the archive, rename its `ish-*`
executable to `ish`, and place it in a directory on your `PATH`, such as
`~/.local/bin`.

Start the shell with:

```sh
ish
```

Use `ish --help` for command-line options. `-c PATH` selects a config file,
`--no-config` skips config loading, and `-V` prints the version.

## Configuration

By default, ish reads `~/.config/ish/config.ish`. If `HOME` is unset, it checks
`/etc/ish/config.ish`. Use `-c PATH` to select another file.

The file supports `set` and `alias` directives. A `set` value is one shell word;
quote values that contain spaces. `$NAME` is expanded when the config loads.
Lines beginning with `#` are comments. Invalid lines are reported and do not
prevent later lines from loading. A nearby `init.sh` is not run.

```sh
set EDITOR nvim
set GREETING "hello $USER"
alias ll l
alias gs git status
alias .. "cd .."
```

## Interactive use

The prompt shows the user, host, shortened working directory, Git branch, and
last command status. A red `*` marks a dirty denv environment. After you run
`git init` in the current directory, the next prompt shows its branch.
Control characters in the user, host, directory, and branch appear as `�`.

| Key | Action |
|---|---|
| Up / Down | Move through wrapped input; search history at the input boundary |
| Ctrl+R | Search history |
| Tab | Complete commands, hosts, files, or directories |
| Ctrl+A / Ctrl+E | Move to the start / end of the line |
| Ctrl+W / Ctrl+Delete | Delete the previous word |
| Alt+D | Delete the next word |
| Ctrl+U / Ctrl+K | Delete to the start / end of the line |
| Ctrl+Y | Restore the last killed text |
| Ctrl+Backspace | Pick a recent directory |
| Left / Right | Move by character; Ctrl+Left / Ctrl+Right move by word |
| Ctrl+C / Ctrl+D | Cancel input / delete forward or exit on an empty line |
| Ctrl+L | Clear the screen |
| Ctrl+Z | Suspend the foreground job |
| Ctrl+P | Write a layout dump under `~/.cache/ish/` |

Completion filters as you type and prefers prefix matches. Recent files appear
first; partial paths and SSH hosts are supported. SSH host completion reads
`~/.ssh/config` and `~/.ssh/known_hosts`. Use the arrows or Tab to move through
results, Enter to accept, and Escape to cancel.

Ctrl+R opens a case-insensitive fuzzy-search pager with matches highlighted.
Typing or pasting edits the search query and filters the matches.
Use Up and Down to navigate, Enter to accept, and Escape to cancel. History is
saved in `~/.local/share/ish/history.log`; Ctrl+R searches saved commands across
sessions, while Up and Down use this session's history snapshot. `history`
prints the list and `history reset` clears it. The first run after an upgrade
imports the old `history` and `history.bin` files and deletes them. Run `history -h` for help.

## Commands and syntax

ish accepts pipelines with `|` or `|&`, lists joined by `&&`, `||`, or `;`,
command negation with `!`, and file redirections such as `2>errors.txt` and
`&>output.txt`. It supports quoting, escaping, comments, continuation lines,
tilde, parameter and arithmetic expansion, command substitution, and pathname
expansion (globbing). An unmatched quote or a trailing `|`, `&&`, or `||`
requests another input line.

```sh
ls -la | grep src && echo found > result.txt
cat 'literal $HOME' "expanded $HOME"
echo ~/projects/*.rs
echo $((1 + 2))
! false || echo fallback
```

`${NAME}` is accepted alongside `$NAME`; parameter operators such as
`${NAME:-default}` are refused. Background `&`, scripts, conditionals (`if`,
`for`, `while`, `case`), subshells, brace groups, functions, `source`, `.`, and
`eval` are not supported. ish checks the command target after expansion too.
If expansion selects `eval`, `source`, `.`, or a shell-state builtin outside
its allowed whole-line form, ish refuses the command with status 2 before that
target runs or opens its redirections. Earlier commands in the input line keep
their effects. Commands that need shell state, including `alias`, `exit`, `fg`,
`denv`, and `z`, must be the whole input line. `cd` and `history` can also be
used in lists. `t` and `trash` must also be the whole input line.

Native `cp` and `mv` run in the shell process, including lists, redirections,
and standalone negation. `! cp source copy` performs the copy and inverts its
status. Genuine pipelines and command substitutions refuse native operations
before target redirections or filesystem effects. Refusal returns 2; an
expression's `!` can invert that status while retaining the diagnostic.
Argument substitutions are allowed. Aliases after `!` expand before dispatch.
Use `command cp` or an alias for the same native operation. Use an explicit
path such as `/bin/cp` to run the external utility.

Arithmetic follows [epsh's signed 64-bit rules](crates/epsh/README.md#arithmetic).

A bare directory name becomes an implicit `cd` only when it does not match a
builtin, alias, or executable on `PATH`. Use `cd name` to enter a directory
when a command has the same name.

ish-specific commands include:

| Command | Action |
|---|---|
| `cd [dir]` | Change directory; `cd -` returns to the previous directory |
| `exit [status]` | Exit, warning if a job is suspended |
| `fg` | Resume the suspended foreground job |
| `alias [name [command...]]` | Define or list aliases |
| `history [reset]` | List or clear saved history |
| `z [query]` | Jump to a frequently used directory; no query goes home |
| `cp [OPTIONS] SOURCE DESTINATION` | Copy files or directory trees |
| `mv [OPTIONS] SOURCE DESTINATION` | Move files or directory trees |
| `t`, `trash` | Permanently delete entries below an explicit directory; `--yes` skips confirmation |
| `w`, `which`, `type` | Locate a command or alias |
| `l [path...]` | List files with colors and human-readable sizes |
| `c` | Clear the screen |
| `denv` | Trust, revoke, or reload environment files for this directory |
| `copy-scrollback` | Copy the terminal scrollback through OSC 52 |
| `?` | Copy the previous command's output through OSC 52 |
| `ish-dump` | Write the current layout dump |

`command -v` and `command -V` identify registered ish commands as builtins even
when `PATH` is empty. A command path such as `/bin/cp` is reported and run as
the external program.

`t PATH` and `trash PATH` scan the directory tree before deleting anything.
They permanently delete its entries and keep `PATH` itself. The last component
of `PATH` must name a directory, not a symlink. The command refuses
`/`, `$HOME`, and any parent of the current directory. It removes nested
symlinks themselves and leaves their targets alone. An incomplete scan or mount
boundary stops deletion before it starts. By default, the command asks you to
type `yes`. Pass `--yes` to skip the prompt. Without a terminal, `--yes` is
required. A terminal shows scan progress and a deletion progress bar.
If deletion is interrupted, entries may remain in a `.fdu-quarantine-v1-*`
directory inside `PATH`. The next `t PATH` restores them and reports
"interrupted deletion recovered; rescan before deleting"; run it again to
delete. A damaged or unrelated directory with that prefix blocks deletion until
you move it away.

ish's `cp` and `mv` use an exact-target form: the source object maps to the
destination path. For example, `cp report.txt .` reports a type conflict when
`.` is a directory. Use `cp --into . report.txt` to place the file inside it.
`--into DIRECTORY` maps each source to a child named after its basename.
Directories recurse without `-r`; `cp -r` and `cp -R` are accepted as redundant
flags. Read the [file operation reference](docs/filesystem.md) for collision,
preservation, mount, durability, cancellation, and partial-completion rules.
Unsupported required metadata is reported as a failure and the source is
retained. Explicit paths such as `/bin/cp` and `/bin/mv` still run external
utilities.

The shell also provides epsh's environment and output builtins, including
`set`, `export`, `unset`, `echo`, `pwd`, `true`, and `false`. Aliases can be
defined at the prompt or in config and expand as you type.

### Copying command output

`?` must be the whole line. It copies the text the previous command wrote to the
terminal through OSC 52, so the terminal must allow clipboard writes. Every other
accepted line replaces the saved output, including shell builtins such as `cd`.
`?` does not, and `fg` continues the suspended job's output. A line ish refuses
before running leaves the saved output alone.

- Standard output and standard error are saved together in the order they were
  written. Every stage of a pipeline and every command in a list contributes
  what reaches the terminal.
- A redirection or pipe that takes a stream away from the terminal also takes
  it out of the saved output. `echo x > file` saves nothing.
- Terminal control sequences such as colors and cursor movement are removed from
  the copy, and `\r\n` becomes `\n`. Output from full-screen programs is not
  useful to copy.
- ish keeps the last 1 MiB. Larger output copies only its end, starting after
  the first newline in what remains, and `?` says so.
- With nothing to copy, `?` prints a message and returns 1.

While a command runs, its stdout and stderr are a pseudo-terminal that ish
relays to your terminal, so programs still see terminals there. The relay
follows window resizes within about 200 ms. A command that reads its size on
the first `SIGWINCH` can see the old size, so the relay sends it a second
`SIGWINCH` once the size is updated.
That pseudo-terminal is not the command's controlling terminal: input, Ctrl+C,
and Ctrl+Z still go through your terminal. Output the command writes to
`/dev/tty` directly is not saved. A background process that outlives its command
loses its output once ish stops relaying, because its writes then fail. If the
pseudo-terminal cannot be created, ish reports it and runs the command without
saving its output.

### Job control

Ctrl+C interrupts foreground commands, including pipelines.
Ctrl+Z suspends a foreground job and `fg` resumes it. ish keeps one suspended
job. If a command in a list suspends, the rest of that list is discarded.
During native `cp` and `mv`, ish ignores Ctrl+Z and continues the operation.

### denv

When the directory changes, ish looks for `.envrc` and `.env`. Run
`denv allow` to trust the files in the current directory, `denv deny` to revoke
trust, and `denv reload` to reevaluate them. Editing a trusted file revokes its
trust. If either discovered file is untrusted, neither is loaded. `.envrc`
files whose first line mentions `bash` run in a `bash` child; other `.envrc`
files are evaluated by epsh. `.env` files are read as variable assignments.
Evaluation stops after 60 seconds.

`denv` fixes its trust and restoration storage directory when the shell starts,
using `DENV_DATA_DIR` or `$HOME/.local/share/denv`. Project changes to `HOME` or
`DENV_DATA_DIR` do not move that storage. Leaving a project restores its variables.
Each shell keeps its restoration record in that directory under a name made of
its pid, on Linux its pid namespace, and a random token, exported as `__DENV_PID`, and removes the record
when it exits. A shell killed without cleanup leaves its record behind; no later
shell reads it, and each new shell removes records whose pid is no longer
running in the new shell's pid namespace. A record whose pid has been reused by
a running process stays, and so does a record written from another pid
namespace, such as another container sharing the directory.
If the shell's active restoration record is malformed or
uses an unsupported version, `denv` reports an error and leaves the process
environment unchanged. `denv reload`, `denv allow`, and `denv deny` then
discard the record and report that the variables it saved cannot be restored.

## Help and license

Report issues at [GitHub Issues](https://github.com/joshuarli/ish/issues). This
repository does not specify a general license. The `ish-fs` crate is MIT
licensed; see [its license](crates/ish-fs/LICENSE) and
[import note](crates/ish-fs/IMPORT.md).
