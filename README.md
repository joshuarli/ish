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
| Left / Right | Move by character |
| Ctrl+Left / Alt+B | Move word left |
| Ctrl+Right / Alt+F | Move word right |

Single kill ring shared across Ctrl+K/U/W. Full UTF-8 support.

**Multiline editing**: paste a multi-line command (with `\` continuations or unclosed quotes) and move freely with Up/Down between lines. No more unwinding — the entire command lives in one buffer with free cursor movement.

### History

| Key | Action |
|---|---|
| Up / Down | Prefix search through history |
| Ctrl+R | Fuzzy search (subsequence, case-insensitive) |

Fuzzy search opens a pager with matching characters highlighted in yellow. Up/Down to navigate, Enter to accept, Escape to cancel.

**Scored ranking**: results are ranked by match quality, not just recency. Entries recorded in the current directory or one of its ancestors get a priority boost. Contiguous matches (searching "target" finds literal `target/release/` first), word-boundary alignment (`deb` prefers `debug/` over scattered d-e-b), and optimal alignment via forward+backward scan find the tightest match window — "test" in "the best test" finds the contiguous "test" at the end, not scattered letters.

Stored at `~/.local/share/ish/history`. Deduplicated on add.

`history reset` deletes all saved history and invalidates cached history in
other running ish shells.

Run `history -h` for the available history storage commands.

### File Finder

| Key | Action |
|---|---|
| Ctrl+F | Open file finder |
| Down (in query) | Toggle hidden mode |
| Enter | Search / accept selection |
| Up / Down (in results) | Navigate |
| Backspace (in results) | Back to query |
| Escape | Cancel |

Type a pattern, press Enter, pick from results. The selected path is inserted at the cursor position — type `vim `, press Ctrl+F, pick a file, get `vim src/main.rs`.

Native implementation: recursive readdir with gitignore support, no external dependencies. Results sorted by depth (shallowest first) so nearby files appear before deeply nested ones. Normal mode respects `.gitignore`; hidden mode shows everything except `.git/`.

Sub-millisecond for gitignore-respecting searches (~0.85ms). No indexes, no background processes.

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
| `ish-dump` | Write the current layout state to `~/.cache/ish/dump-<random hex>` |

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

Single Rust package with a library target and interactive binary. Platform code primarily uses `rustix`; `libc` remains a narrow compatibility escape hatch.

Shell-owned operations stay native where possible: directory listing, git detection, pathname expansion, completion, and file finding do not spawn subprocesses. Running external commands, command substitutions, and the denv integration may spawn processes.

Signal handling uses the self-pipe pattern. Each pipeline gets its own process group. Terminal foreground control via `tcsetpgrp`. Raw mode via `termios`.
