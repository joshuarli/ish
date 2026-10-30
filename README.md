# ish

ish is an interactive shell for macOS and Linux. It provides a native prompt,
line editing, history search, completion, aliases, minimal job control, native
`cp` and `mv` commands, and protected deletion. It has no script-file or
command-string mode.

## Install and run

Download a pre-release archive for your platform from
[GitHub Releases](https://github.com/joshuarli/ish/releases). The release
workflow provides macOS Apple silicon and statically linked Linux x86_64 and
ARM64 builds. Extract the archive, rename its `ish-*`
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

Completion filters as you type or paste and prefers prefix matches. Recent
files appear first; partial paths and SSH hosts are supported. SSH host
completion reads `~/.ssh/config` and `~/.ssh/known_hosts`. Use the arrows or Tab
to move through results, Enter to accept, and Escape to cancel. Pasting while
the recent-directory picker is open leaves its saved input unchanged.

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
Unavailable builtin names and invalid `set` forms are refused with status 2,
including targets selected through expansion or nested `command`. Static
violations prevent the input line from starting. Expanded targets are checked
before their redirections, assignments for dispatch, or execution; earlier
commands and expansions keep their effects. Commands that need shell state,
including `alias`, `exit`, `fg`, `denv`, and `z`, must be the whole input line.
`cd` and `history` can also be used in lists. `t` and `trash` must also be the
whole input line.

Native `cp` and `mv` run in the shell process, including lists, redirections,
and standalone negation. `! cp source copy` performs the copy and inverts its
status. Genuine pipelines and command substitutions refuse native operations
before target redirections or filesystem effects. Refusal returns 2; an
expression's `!` can invert that status while retaining the diagnostic.
Argument substitutions are allowed. Aliases after `!` expand before dispatch.
Use `command cp` or an alias for the same native operation. Use an explicit
path such as `/bin/cp` to run the external utility.

Native `l`, `ll`, `c`, and the list form of `history` also require the ish process.
They refuse pipeline stages and command substitutions with status 2 before
target redirections. Their host callbacks have no child-execution safety
contract. Ordinary external programs, epsh builtins, and hybrid `cd` retain
their pipeline and substitution forms.

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
| `cp [OPTIONS] SOURCE DESTINATION` | Copy files or directory trees; with more than one source the last operand must be a directory |
| `mv [OPTIONS] SOURCE DESTINATION` | Move files or directory trees; with more than one source the last operand must be a directory |
| `t`, `trash` | Permanently delete entries below an explicit directory; `--yes` skips confirmation |
| `w`, `which`, `type` | Locate a command or alias; a same-name alias also shows the underlying command |
| `l [path...]` | List visible files with sizes and relative ages |
| `ll [path...]` | List all files including hidden ones |
| `c` | Clear the screen |
| `denv` | Trust, revoke, or reload environment files for this directory |
| `copy-scrollback` | Copy the terminal scrollback through OSC 52 |
| `?` | Copy the previous command's output through OSC 52 |
| `ish-dump` | Write the current layout dump |

`command -v` and `command -V` identify registered ish commands as builtins even
when `PATH` is empty. A command path such as `/bin/cp` is reported and run as
the external program. Unavailable epsh builtin names are absent from command
completion and lookup, even if a same-named executable exists on `PATH`. Use
an explicit path to run that executable. `command -v exit` can identify the
whole-line ish command; `command exit` refuses its execution.

`t PATH` and `trash PATH` scan the directory tree before deleting anything.
They permanently delete its entries and keep `PATH` itself. The last component
of `PATH` must name a directory, not a symlink. The command refuses
`/`, `$HOME`, and any parent of the current directory. It removes nested
symlinks themselves and leaves their targets alone. An incomplete scan or mount
boundary stops deletion before it starts. By default, the command asks you to
type `yes`. Pass `--yes` to skip the prompt. Without a terminal, `--yes` is
required. A terminal shows scan progress and a deletion progress bar.
If deletion is interrupted, entries may remain in a `.fdu-quarantine-v2-*`
directory inside `PATH`. The next `t PATH` restores verified captures without
overwriting live names and requires a fresh scan and retry. Existing v1 sessions
retain their restoration behavior. Damaged records, conflicting names, and
unrelated quarantine lookalikes block deletion and identify the retained paths.
An entry removed before a synchronization error is reported as removed with a
durability failure.

ish's `cp` and `mv` follow the standard operand forms: one source maps to
`DESTINATION`, while several sources map into the last operand, which must be
an existing directory. For example, `cp report.txt .` places the file inside
`.` as `./report.txt` when `.` is a directory, and `mv src/* .` moves every
match into `.`. `--into DIRECTORY` is the explicit spelling of the multi-source
form: `cp --into . report.txt` matches `cp report.txt .`, and
`mv --into . src/*` matches `mv src/* .`. Each source maps to a child named
after its basename.
Directories recurse without `-r`; `cp -r` and `cp -R` are accepted as redundant
flags. Read the [file operation reference](docs/filesystem.md) for collision,
preservation, mount, durability, cancellation, and partial-completion rules.
Unsupported required metadata is reported as a failure and the source is
retained. Explicit paths such as `/bin/cp` and `/bin/mv` still run external
utilities. Interrupted copies and moves use the same quarantine protocol.
On macOS, filesystem staging does not support an inherited umask that removes
owner read or search permission (mask bits `0500`); for example, `0777` is
unsupported. ish reports a failure and keeps filesystem operations in-process.
Unpublished stages remain available for inspection. Recovery preserves published
destinations and never resumes deleting sources. Read the
[recovery reference](docs/filesystem.md#inspect-retained-operation-state) before
retrying an interrupted operation.

The exposed epsh builtins are `echo`, `printf`, `pwd`, `true`, `false`, `:`,
`test`, `[`, `kill`, `export`, `unset`, `set`, and `command`; `cd` also retains
its hybrid form. Other epsh builtins, including `exec`, `trap`, `read`, `wait`,
`umask`, and `getopts`, are unavailable. This boundary does not restrict programs
run through explicit paths.

`set` lists exported variables. `set NAME [value...]` sets and exports a
variable, joining values with spaces; no values gives an empty value. The name
must match `[A-Za-z_][A-Za-z0-9_]*`. Values may start with `-` or `+`. Option
and positional forms, including `set -e`, `set +e`, `set -o pipefail`, and
`set --`, are refused.
Aliases can be defined at the prompt or in config and expand as you type.

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

While a command runs, stdout and stderr that would reach the terminal go
through a pseudo-terminal that ish relays to your terminal. The relay applies
a window resize within about 200 ms and signals `SIGWINCH` when the size
changes. A single simple command uses that pseudo-terminal as its controlling
terminal and receives what you type. Pipelines and lists do not, so a program
in them that reads the output terminal for input waits forever. Further rules
are in [Output capture](docs/architecture.md#output-capture). A process that
outlives its command loses later output once relaying stops. If the
pseudo-terminal cannot be created, ish reports it and runs the command without
saving its output.

### Job control

Ctrl+C interrupts foreground commands, including pipelines. A SIGINT sent
to the shell (for example `kill -INT`) cancels the line being edited.
Ctrl+Z suspends a foreground job and `fg` resumes it. ish keeps one suspended
job. If a command in a list suspends, the rest of that list is discarded.
The suspended job retains every pipeline member and its output capture.
`fg` waits for the whole group and uses the pipeline's final-stage status.
An external command that exits with status 148 does not create a suspended job.
During native `cp` and `mv`, ish ignores Ctrl+Z and continues the operation.

### denv

When the directory changes, ish looks for `.envrc` and `.env`. Run
`denv allow` to trust the files in the current directory, `denv deny` to revoke
trust, and `denv reload` to reevaluate them. Editing a trusted file revokes its
trust. If either discovered file is untrusted, neither is loaded. `.envrc`
files whose first line mentions `bash` run in a `bash` child; other `.envrc`
files are evaluated by epsh. `.env` files are read as variable assignments
after `.envrc`, so `.env` values take precedence. Shell syntax in `.env`
values remains data.
Evaluation stops after 60 seconds.

Trust covers the exact bytes of the discovered files. An approved `.envrc`
runs arbitrary code with your authority. Explicit sourcing and newly executed
interpreters can load other files without denv approval or per-file revocation.

`source_env`, `source_env_if_exists`, `source_up`, `source_up_if_exists`,
`dotenv`, and `dotenv_if_exists` are removed. Calling any of them rejects the
whole candidate environment, including inside substitutions, pipelines, or
subshells, even if a later command succeeds. Remove redundant `dotenv` calls
and approve the automatically discovered `.env` with `denv allow`. Put shared
setup in the discovered `.envrc` and approve its updated contents. Rejection
prevents candidate variables from entering ish. External effects already
performed by trusted code cannot be undone.

Shell variables and exported child environments belong to epsh's variable store.
ish imports its inherited environment at startup and never writes changes back
to its own process environment. Evaluators and commands receive explicit
byte-preserving environments; denv locates projects from the shell's actual cwd,
independently of the value assigned to `PWD`.

Denv validates a complete candidate, including readonly bindings, and persists
its restoration record before publishing the candidate. Assignment or storage
errors leave the current layer installed. A rejected script installs none of
its assignments; denv restores the prior layer and marks the project dirty.

`denv` fixes its trust and restoration storage directory when the shell starts,
using `DENV_DATA_DIR` or `$HOME/.local/share/denv`. Project changes to `HOME` or
`DENV_DATA_DIR` do not move that storage. Leaving a project restores its
variables, including raw values, empty versus unset bindings, and whether each
binding was exported.
Each shell keeps its restoration record in that directory under a name made of
its pid, on Linux its pid namespace, and a random token, exported as `__DENV_PID`, and removes the record
when it exits. A shell killed without cleanup leaves its record behind; no later
shell reads it, and each new shell removes records whose pid is no longer
running in the new shell's pid namespace. A record whose pid has been reused by
a running process stays, and so does a record written from another pid
namespace, such as another container sharing the directory.
If the shell's active restoration record is malformed or
uses an unsupported version, `denv` reports an error and leaves shell variables
unchanged. `denv reload`, `denv allow`, and `denv deny` then
discard the record and report that the variables it saved cannot be restored.

## Help and license

Report issues at [GitHub Issues](https://github.com/joshuarli/ish/issues). This
repository does not specify a general license. The `ish-fs` crate is MIT
licensed; see [its license](crates/ish-fs/LICENSE) and
[import note](crates/ish-fs/IMPORT.md).
