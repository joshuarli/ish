# ish — Minimal Interactive Shell

## Overview

A personal interactive-only shell inspired by fish-shell. Keeps only the features
that matter: native prompt with git branch, fast fuzzy history search, grid-based
file tab completion, alias autoexpansion, and minimal job control. Written in Rust
with a single dependency (`libc`). No scripting, no POSIX compat, no runtime
extensibility.

**Hard constraint: the shell never subprocesses out for its own operations.**
Directory listing, git branch detection, glob expansion, clipboard — all
implemented natively. Only user commands get fork/exec'd.

## File Layout

```
src/
  main.rs        — entry point, shell loop, ShellState
  term.rs        — raw mode (termios), terminal size, write helpers
  input.rs       — byte reader, VT100 escape decoder → KeyEvent
  line.rs        — line editing buffer, cursor, kill ring
  prompt.rs      — native prompt (ported from fish native_prompt.rs)
  render.rs      — composites prompt + line + completions + history pager → VT100
  parse.rs       — single-pass tokenizer/parser → CommandLine (no AST)
  expand.rs      — tilde, $VAR, $(), backtick, glob expansion
  exec.rs        — fork/exec, pipe plumbing, redirections, process groups
  builtin.rs     — cd, exit, fg, set, unset, alias, l, c, w, copy-scrollback
  ls.rs          — native dir listing (l builtin): stat, permissions, owner/group,
                    human sizes, color output — like ls -plAhG, no forking
  history.rs     — append-only file, Vec<String> in memory, prefix + fuzzy search
  complete.rs    — file-only readdir, grid layout (column-major), arrow nav
  alias.rs       — alias storage, inline expansion on space
  config.rs      — parse ~/.config/ish/config.ish (set + alias)
  job.rs         — single suspended job (Ctrl+Z / fg)
  signal.rs      — self-pipe pattern, SIGINT/SIGTSTP/SIGCHLD/SIGWINCH
  error.rs       — shell error type
```

17 source files. Single binary crate. One dependency: `libc`.

---

## Core Data Structures

### KeyEvent

`Key` enum + `Modifiers` (ctrl/alt/shift):

```
Key = Char(char) | Up | Down | Left | Right | Home | End
    | Tab | Backspace | Delete | Enter | Escape
```

### LineBuffer

`String` + cursor (byte offset) + kill ring (`String`).

Methods: insert/delete char, word operations (Ctrl+W/U/K/Y), cursor movement
(char-wise, word-wise, home/end).

### CommandLine

Flat structure, no AST:

```
CommandLine { segments: Vec<(Pipeline, Option<Connector>)> }
Pipeline    { commands: Vec<PipedCommand> }
PipedCommand { cmd: Command, pipe_stderr: bool }  // pipe_stderr=true for &|
Command     { argv: Vec<String>, redirects: Vec<Redirect> }
Connector   = And | Or | Semi
```

### Redirections

| Syntax | Meaning                          |
|--------|----------------------------------|
| `>`    | stdout to file (write)           |
| `>>`   | stdout to file (append)          |
| `<`    | stdin from file                  |
| `2>`   | stderr to file                   |
| `&>`   | both stdout+stderr to file       |
| `&\|`  | pipe both stdout+stderr to next  |

### History

`Vec<String>` (deduped — only most recent instance kept) + `PathBuf` +
open `File` for appending. Plain text format, one entry per line. Stored at
`~/.local/share/ish/history`.

No `history` builtin — users edit the file directly.

### CompletionState

`Vec<CompEntry>` + selected index + grid dimensions (cols/rows) + scroll offset.

### Job

Single `Option<Job>` with pid, pgid, command text.

### ShellState

Owns all of the above plus:
- `aliases: AliasMap`
- `last_status: i32`
- `prev_dir: Option<String>` (for `cd -`)
- Terminal dimensions (rows, cols)
- PATH cache (`HashMap<String, PathBuf>`) — invalidated on `cd` or `set PATH`
- `exit_warned: bool` — for the suspended-job exit warning

---

## Builtins

| Command           | Behavior                                                      |
|-------------------|---------------------------------------------------------------|
| `cd [dir]`        | Change directory. No args → `$HOME`. `cd -` → previous dir.  |
| `exit`            | Exit shell. Warns if suspended job exists; second `exit` kills and exits. |
| `fg`              | Resume single suspended job in foreground.                    |
| `set VAR "val"`   | Set env var. `set VAR` with no value → set to empty string.  |
| `unset VAR`       | Delete env var entirely.                                      |
| `alias name cmd [args...]` | Define alias. Alias can shadow builtins.             |
| `l [path]`        | Native dir listing (see ls.rs section). Accepts one optional path. |
| `c`               | Clear screen. `ESC[2J` + cursor home. Preserves scrollback.  |
| `w name`          | Like `command -v`. Output format (see below).                 |
| `copy-scrollback` | Copy terminal scrollback to system clipboard via OSC 52.      |

### `w` output format

```
w cd       → builtin
w l        → builtin
w tmux     → alias: tmux -f /path/to/tmux.conf new -s
w rg       → /opt/homebrew/bin/rg
w nope     → ish: w: not found: nope    (exit status 1)
```

Checks order: alias → builtin → PATH lookup.

### `l` — Native Directory Listing

Implements `ls -plAhG` entirely within the shell (no fork/exec):

- `readdir` to enumerate entries, skip `.` and `..` (`-A`)
- `lstat` each entry for metadata (shows symlinks as links, not targets)
- Format: `permissions nlink owner group size date name[/]`
- Symlinks: `name -> target`
- Human-readable sizes (`-h`): K, M, G
- Append `/` to directories (`-p`)
- Color output (`-G`): dirs=blue, symlinks=cyan, executables=green/red
- Owner/group via `getpwuid`/`getgrgid` (libc, no subprocess)
- Sort entries lexicographically (case-insensitive)
- Accepts one optional path argument (defaults to cwd)

---

## Line Editing

All standard keybinds:

| Key                    | Action                                       |
|------------------------|----------------------------------------------|
| Left / Right           | Move cursor one char                         |
| Home / Ctrl+A          | Beginning of line                            |
| End / Ctrl+E           | End of line                                  |
| Ctrl+Left / Alt+B      | Move word backward                           |
| Ctrl+Right / Alt+F     | Move word forward                            |
| Backspace              | Delete char before cursor                    |
| Delete / Ctrl+D        | Delete char at cursor (or exit on empty line)|
| Ctrl+W                 | Delete word backward (into kill ring)        |
| Ctrl+U                 | Delete to beginning of line (into kill ring) |
| Ctrl+K                 | Delete to end of line (into kill ring)       |
| Ctrl+Y                 | Yank (paste from kill ring)                  |
| Ctrl+C                 | Cancel current line, fresh prompt            |
| Ctrl+L                 | Clear screen (same as `c` builtin)           |
| Up / Down              | History navigation (see History section)     |
| Tab                    | Trigger completion (see Completion section)  |
| Enter                  | Execute line                                 |

---

## Input Decoding

Byte-at-a-time from stdin. ESC followed by poll (50ms timeout):
- No follow-up byte → Escape key
- `[` → CSI sequence: parse numeric params + final byte for arrows, modifiers,
  home/end/delete
- `O` → SS3: arrows, home/end
- Other byte → Alt+char

UTF-8 multi-byte: on leading byte (0x80+), read expected continuation bytes.

---

## Command Parsing

Single-pass scanner. No nesting, no recursion (except for `$()` / backtick
detection). Char-by-char with quoting state:

- **Unquoted**: all expansions active, whitespace splits tokens
- **Single-quoted** (`'...'`): everything literal, no expansion
- **Double-quoted** (`"..."`): `$VAR` and `$(...)` / `` `...` `` expanded,
  everything else literal
- **Backslash** (`\`): escapes next char in unquoted and double-quoted contexts

Produces `Token` stream:
`Word | Pipe | PipeStderr | And | Or | Semi | Redirect`

Assembled into `CommandLine`. Expansion applied post-parse, pre-exec.

### Continuation Lines

If a line ends with `|`, `&&`, `||`, or an unclosed quote, prompt for
continuation. Continuation prompt is indentation only (no visible prompt
characters). Lines are concatenated before parsing.

---

## Expansion Order

Applied post-parse, pre-exec, in this order:

1. **Tilde** — `~` → `$HOME` (only at start of word or after `=`)
2. **Variable** — `$VAR` → env var value
3. **Command substitution** — `$(cmd)` and `` `cmd` `` → stdout of cmd
4. **Glob** — `*`, `?`, `**` patterns → matching filenames

### Command Substitution

Both `$(...)` and `` `...` `` are supported. Nesting is supported in both forms:
- `$(echo $(whoami))` — `$()` nests naturally via paren counting
- `` `echo \`whoami\`` `` — backticks nest via escaped inner backticks

Nesting is validated before execution. Substitution captures stdout, trims
trailing newlines, splits on whitespace in unquoted context.

### Glob Expansion

Supports:
- `*` — any chars within one path segment
- `?` — single char within one path segment
- `**` — recursive descent through directories

Example: `*/**/*.py` matches Python files at any depth under immediate subdirs.

Implemented natively via recursive `readdir` — no subprocessing. When a pattern
contains `**`, walk the directory tree recursively; otherwise single-level
`readdir`.

**No matches → error.** The shell prints an error and does not execute the
command (like fish, unlike bash).

---

## History

### Storage

- File: `~/.local/share/ish/history`
- Format: plain text, one entry per line
- Loaded into `Vec<String>` on startup
- Appended to file after each successful parse
- Deduplication: when adding a new entry, remove all prior occurrences from the
  in-memory list (file is append-only; dedup happens on load and in memory)

### Navigation

- **Up/Down with empty line**: cycle through full history (most recent first)
- **Up/Down with text**: prefix match — linear scan backward, skip duplicates
- **Ctrl+R**: fuzzy subsequence search with pager UI

### Ctrl+R Pager

Display: search field at top, matches below. UI:
- Type to filter (subsequence match: every char of query appears in entry in
  order, case-insensitive)
- Matching characters highlighted in results
- Up/Down to cycle through matches
- Enter to accept (insert into line buffer)
- Escape to cancel (return to normal editing)

---

## Tab Completion

### Trigger

- **Tab with text**: complete the word at cursor as a file path
- **Tab on empty line**: auto-insert `cd ` and open directory completions from cwd

### Behavior

- **Single match**: auto-insert directly (no grid). For directories, append `/`
  and continue completing (don't close completion mode).
- **Multiple matches**: show grid, enter completion navigation mode.

### Grid Layout

Column-major ordering. Algorithm:
1. Try cols = min(6, N) down to 1
2. Compute column widths from entries in that column
3. Check if total width fits terminal
4. Entry at visual position (row, col) = `entries[col * rows + row]`

Navigation:
- Arrow keys move through grid
- Wrapping: Up from row 0 → last row of previous column, etc.
- Enter or Tab → accept selection
- Escape → cancel, return to normal editing
- Typing continues to filter completions

### Display

- Directories shown with trailing `/` and colored blue
- Symlinks colored cyan
- Executables colored green
- Regular files uncolored

---

## Aliases

### Definition

In config file or at runtime:
```
alias name cmd [args...]
```

Aliases can shadow builtins.

### Expansion

- **On space press**: if the first word of the current line matches an alias,
  replace it inline with the expansion (visual feedback to user)
- **On exec**: same check. Alias args prepended to user's args.
- **No recursive expansion**: if an alias expands to something starting with
  another alias name, do not expand again (prevents infinite loops).

---

## Prompt

Layout: `user@host colored_pwd[*] [branch] $ `

### Components

- **user@host**: computed once on startup, cached
- **PWD**: tilde-contract `$HOME`, abbreviate middle components to first char
  (e.g., `~/.config/fish` → `~/.c/fish`, `~/dev/fish-shell` → `~/d/fish-shell`).
  `.` prefix preserved (`.config` → `.c`).
- **Color**: green when `last_status == 0`, red otherwise
- **Git branch**: walk ancestor directories for `.git/`, read `HEAD`, three-way
  cache (`Repo(branch)` / `NoRepo` / `Unknown`). Cache invalidated on `cd`.
- **Dirty indicator**: red `*` displayed when env var `__DENV_DIRTY=1`

---

## Signal Handling

Self-pipe pattern: signal handlers write signal ID byte to a pipe. Main loop
`poll()`s both stdin and the pipe read-end.

### Process Groups

- Each pipeline gets its own pgid (`setpgid(0, 0)`)
- Shell gives foreground to pipeline via `tcsetpgrp`
- Shell reclaims foreground on pipeline completion or suspension

### Signals

| Signal   | Shell behavior                                    |
|----------|---------------------------------------------------|
| SIGINT   | Delivered to foreground process group. Shell ignores for itself. |
| SIGTSTP  | Shell ignores. Foreground job suspended → stored in job slot.    |
| SIGCHLD  | Reaped via self-pipe → main loop.                 |
| SIGWINCH | Terminal size updated via self-pipe → main loop.  |
| SIGTTOU  | Shell ignores (needed for `tcsetpgrp`).           |
| SIGTTIN  | Shell ignores.                                    |
| SIGQUIT  | Shell ignores.                                    |
| SIGPIPE  | Shell ignores.                                    |

---

## Job Control

Minimal: single job slot.

- **Ctrl+Z**: suspend foreground process, store in job slot
- **`fg`**: resume suspended job, give it foreground
- **No `&`**: background execution not supported
- **Exit with job**: first `exit` prints warning ("there is a suspended job"),
  sets `exit_warned` flag. Second `exit` kills the job (`SIGTERM` then
  `SIGKILL`) and exits.

---

## Config File

Path: `~/.config/ish/config.ish`

Read on each startup. Only two directives:

```
set VAR "value"       # set environment variable (value can contain $VAR refs)
alias name cmd [args...]   # define alias
```

- Variables in values are expanded at parse time (`"$HOME/bin"` → `/Users/josh/bin`)
- Lines starting with `#` are comments
- Empty lines ignored
- Bad lines: warn to stderr, continue processing remaining lines

---

## PATH Cache

- On startup: scan all `$PATH` directories, build `HashMap<String, PathBuf>`
- On command lookup: check cache first
- Invalidated and rebuilt on: `cd`, `set PATH ...`, `unset PATH`
- Used by `w` builtin and command execution

---

## `copy-scrollback` Builtin

Copies terminal scrollback buffer to system clipboard using OSC 52 escape
sequence:

```
ESC ] 52 ; c ; <base64-encoded-data> BEL
```

Reads the terminal's scrollback via the corresponding OSC 52 query sequence,
then writes it back with the clipboard set operation. This is terminal-native
— no subprocess involved.

Note: requires terminal support for OSC 52 (most modern terminals: iTerm2,
kitty, alacritty, WezTerm, ghostty).

---

## Safety

- **Refuses to run script files**: if invoked with arguments on the command line
  (e.g., `ish script.sh`), print error and exit
- **Refuses to source**: no `source` or `.` command
- **No control flow**: `while`, `for`, `if`, `function` are not recognized as
  keywords — they're treated as command names

---

## Implementation Phases

### Phase 1: Terminal + Input + Line Editor + Minimal Exec

Get a working REPL. Files: `main.rs`, `term.rs`, `input.rs`, `line.rs`,
`exec.rs`, `builtin.rs` (cd/exit/c only), `signal.rs`, `error.rs`.

- Raw mode terminal setup/teardown (termios via libc)
- Byte-by-byte input reading with VT100 escape decoding
- Full line editing (all keybinds from the table above)
- Simple prompt: `$ `
- Fork/exec single commands (no pipes yet)
- PATH lookup (linear scan, no cache yet)
- `cd`, `exit`, `c` builtins
- Ctrl+C cancels line, Ctrl+D exits on empty line
- SIGINT/SIGCHLD handling via self-pipe

### Phase 2: Parsing + Pipes + Redirects + Chaining

`ls -la | grep foo > out.txt && echo done` works. Files: `parse.rs`, updated
`exec.rs`.

- Single-pass tokenizer with quoting (single/double/backslash)
- Pipe plumbing (including `&|` for stderr)
- All redirections (`>`, `>>`, `<`, `2>`, `&>`)
- Connectors: `&&`, `||`, `;`
- Continuation lines (trailing `|`, `&&`, `||`, unclosed quotes)
- Process groups per pipeline, proper foreground management

### Phase 3: Expansion

Tilde, variables, command substitution, globs. Files: `expand.rs`.

- `~` → `$HOME`
- `$VAR` expansion (unquoted and double-quoted)
- `$(cmd)` and `` `cmd` `` with nesting support and validation
- Glob: `*`, `?`, `**` — native readdir, error on no match

### Phase 4: Full Prompt

Port `native_prompt.rs`. Files: `prompt.rs`.

- user@host (cached)
- Abbreviated PWD with tilde contraction
- Git branch detection with three-way cache
- Color: green on success, red on failure
- Dirty indicator (`*`) when `__DENV_DIRTY=1`

### Phase 5: History

Files: `history.rs`, updated `render.rs`.

- Load/save `~/.local/share/ish/history`
- Up/Down: full history and prefix search
- Ctrl+R: fuzzy subsequence pager with highlighted matches
- Deduplication on load and insert

### Phase 6: Tab Completion

Files: `complete.rs`, updated `render.rs`.

- File/directory readdir completions
- Single match → auto-insert; directory → append `/` and continue
- Multiple matches → column-major grid with arrow navigation
- Tab on empty → auto-insert `cd ` and show directory completions
- Color: dirs=blue, symlinks=cyan, executables=green

### Phase 7: Config + Aliases

Files: `config.rs`, `alias.rs`.

- Parse `~/.config/ish/config.ish` (set + alias, with variable expansion)
- Bad lines → warn to stderr, continue
- Alias inline expansion on space (visual feedback)
- No recursive alias expansion
- Aliases can shadow builtins

### Phase 8: Job Control + Remaining Builtins + Polish

Files: `job.rs`, updated `signal.rs`, `builtin.rs`, `ls.rs`.

- Ctrl+Z suspends foreground job → single job slot
- `fg` resumes
- Exit warning for suspended job
- `l [path]` — native dir listing (full `ls -plAhG` implementation)
- `w name` — command lookup display
- `set` / `unset` / `alias` runtime builtins
- `copy-scrollback` — OSC 52 clipboard
- PATH cache with invalidation
- SIGWINCH handling for terminal resize
- Refuse script files and `source`

---

## Verification

- **Each phase**: manual interactive testing
- **Unit tests**:
  - LineBuffer operations (insert, delete, word ops, cursor movement)
  - Key decoding (feed byte sequences → assert KeyEvent)
  - Tokenizer/parser (input string → CommandLine)
  - PWD shortening
  - Subsequence matching
  - Grid layout computation
  - Config parsing
  - Glob matching
  - Alias expansion
- **End-to-end**: `cargo build`, run `./target/debug/ish`, exercise all features
