# ish

ish is an interactive shell for macOS and Linux. It provides a prompt, line
editing, history search, completion, aliases, job control, and in-process
file commands.

ish is interactive. It does not run shell scripts. `ish -c COMMAND` runs a
command with `/bin/sh -c` so tools such as `ssh`, `scp`, `cron`, and `make`
can use ish as a login shell.

## Install

Download a pre-release archive for your platform from
[GitHub Releases](https://github.com/joshuarli/ish/releases), extract it,
rename the executable to `ish`, and place it on your `PATH`.

```sh
ish
```

`ish --help` shows command-line options. `-c COMMAND` runs a command with
`/bin/sh`, `--config PATH` selects a config file, `--no-config` skips config
loading, and `-V` prints the version.

## Configuration

By default, ish reads `~/.config/ish/config.ish`. If `HOME` is unset, it
checks `/etc/ish/config.ish`. Use `--config PATH` to select another file.

The file supports `set` and `alias` directives. Quote values that contain
spaces. `$NAME` is expanded when the config loads. Lines beginning with `#`
are comments.

```sh
set EDITOR nvim
set GREETING "hello $USER"
alias gs git status
alias .. "cd .."
```

## Interactive use

The prompt shows the user, host, shortened working directory, Git branch, and
last command status. A red `*` marks a dirty `denv` environment.

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
| Ctrl+_ / Alt+R | Undo / redo line edits |
| Ctrl+Backspace | Pick a recent directory |
| Left / Right | Move by character; Ctrl+Left / Ctrl+Right move by word |
| Ctrl+Right (end of input) | Accept the next word of the history suggestion; Right accepts the whole suggestion |
| Ctrl+C / Ctrl+D | Cancel input / delete forward or exit on an empty line |
| Ctrl+L | Clear the screen |
| Ctrl+Z | Suspend the foreground job |
| Ctrl+P | Write a layout dump under `~/.cache/ish/` |

Tab completion filters as you type and prefers prefix matches. Recent files
appear first, and SSH host completion reads `~/.ssh/config` and
`~/.ssh/known_hosts`. Use arrows or Tab to move through results, Enter to
accept, and Escape to cancel.

Ctrl+R opens history search. Type to filter, use Up and Down to navigate,
Enter to accept, and Escape to cancel. History is saved in
`~/.local/share/ish/history.log` as a checksummed binary journal. Use `history`
to inspect saved commands and `history reset` to clear them; editing the journal
as text corrupts its records.

## Commands

ish runs external programs and a small set of shell commands. Pipelines (`|`,
`|&`), command lists (`&&`, `||`, `;`), negation (`!`), quoting, escaping,
comments, continuation lines, tilde expansion, parameter and arithmetic
expansion, command substitution, globbing, and redirections are supported.
Background jobs (`&`), control flow, subshells, brace groups, functions,
`source`, `.`, and `eval` are not supported.

| Command | Action |
|---|---|
| `cd [dir]` | Change directory; `cd -` returns to the previous directory |
| `exit [status]` | Exit the shell with `status` (default 0); a non-numeric or extra operand is refused with status 2 |
| `fg` | Resume the suspended foreground job |
| `alias [name [command...]]` | Define or list aliases |
| `history [reset]` | List or clear saved history |
| `z [query]` | Jump to a frequently used directory |
| `l [path...]` / `ll [path...]` | List files; `ll` includes hidden files |
| `c` | Clear the screen |
| `w`, `which`, `type` | Locate a command or alias |
| `cp` / `mv` | Copy or move files and directories in the shell process |
| `rsync` | Synchronize local files and directories in the shell process |
| `rmrf PATH...` | Permanently remove paths |
| `fdu [PATH]` | Browse disk usage interactively, or print a summary when redirected |
| `denv` | Trust, reload, or revoke project environment files |
| `copy-scrollback` | Copy session log text to the clipboard through OSC 52 |
| `ish-dump` | Write the current layout dump |

Some commands must be used as the whole input line, including `alias`, `exit`,
`fg`, `fdu`, `denv`, `z`, `rmrf`, `copy-scrollback`, and `ish-dump`. `cd` and
`history` can also appear in command lists.

A bare directory name changes into that directory when it does not match a
command on `PATH`. Use `cd name` when a command has the same name.

## File commands

`cp` and `mv` follow standard operand forms. With one source, the second
operand is the destination. With several sources, the last operand must be an
existing directory, or use `--into DIRECTORY`. Directories recurse by default.

```sh
cp report.txt ./backup/
cp --into . report.txt
mv src/* .
rmrf build tmp/cache
rsync --into ./dest src assets
```

`rmrf` permanently removes the paths you name. It refuses dangerous targets
such as `/`, `$HOME`, the current directory, and parents of the current
directory. Use an explicit path such as `/bin/cp`, `/bin/mv`, or
`/usr/bin/rsync` to run an external utility.

Interrupted file operations may leave recoverable state on filesystems that
support it. See [File operation reference](docs/filesystem.md) for collision
options, recovery, and platform-specific behavior.

Native `rsync` currently has destructive overlap and destination-symlink
defects, including deletion outside the destination with `--delete`.
Use an explicit external executable while those
[data-safety defects](TODO.md#ish-83--p1--rsync-can-delete-its-source-or-files-outside-its-destination)
remain open. Its deletion path has no journaled recovery.

## Job control and terminal

Ctrl+C interrupts foreground commands. Ctrl+Z suspends a foreground job, and
`fg` resumes it. ish keeps one suspended job.

When supported by your terminal, ish emits working-directory and command
markers (OSC 7 and OSC 133) so the terminal can provide prompt navigation and
command boundaries.

## Project environments (`denv`)

When you change directories, ish looks for `.envrc` and `.env`. `denv allow`
trusts the files in the current directory, `denv deny` revokes trust, and
`denv reload` reevaluates them. Editing a trusted file revokes its trust.

`.envrc` runs as code with your authority. Review it before allowing it.
`.env` is loaded as assignments after `.envrc`. An `.envrc` can call
`dotenv_if_exists` (or `dotenv_if_exists .env` or `dotenv_if_exists ./.env`)
to use the discovered, approved `.env` assignments earlier; it succeeds when
that file is absent. Other paths are rejected and reject the environment.
The final automatic `.env` load still overrides `.envrc` exports. Values are
parsed as data, not executed. Approval covers only the discovered files;
direct sourcing by `.envrc` runs with your authority and is not independently
approved or tracked by denv.

## Help and license

Report issues at [GitHub Issues](https://github.com/joshuarli/ish/issues).
The top-level `ish` project is MIT licensed; see [LICENSE](LICENSE). Bundled
and imported crates and test fixtures may carry their own licenses and
notices; for example, `ish-fs` is MIT licensed with an
[import note](crates/ish-fs/IMPORT.md), and `epsh` retains
[conformance notices](crates/epsh/tests/conformance/NOTICE).
