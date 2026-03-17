# AGENTS.md — Guide for Coding Agents

## What is ish?

ish is a minimal interactive-only shell written in Rust, inspired by fish-shell. It keeps only the features that matter for daily use: a native prompt with git branch, fast fuzzy history search, grid-based tab completion, alias autoexpansion, and minimal job control.

**Hard constraints:**

- **Single dependency**: `libc` (for system calls). No other crates at runtime.
- **No subprocessing for shell operations**: Directory listing, git branch detection, glob expansion, prompt rendering — all implemented natively. Only user commands get fork/exec'd.
- **Interactive only**: No scripting, no POSIX compatibility, no `source`, no control flow (`if`/`for`/`while`/`function`). Refuses to run script files.
- **Single binary crate**: 17 library modules + `main.rs`. No workspace, no proc macros.

## Architecture

```
User types → input.rs (decode bytes → KeyEvent)
           → line.rs  (edit LineBuffer)
           → main.rs  (Enter key pressed)
           → parse.rs (tokenize → CommandLine)
           → exec.rs  (expand → builtin or fork/exec)
           → render.rs (repaint prompt + buffer + completions)
```

### Module Map

```
src/
  main.rs      — Entry point, shell loop, Shell struct, Mode enum, all keybind dispatch
  lib.rs       — Module declarations (17 pub mod statements)
  term.rs      — Raw mode (termios), terminal size, TermWriter (buffered VT100 output)
  input.rs     — Byte reader, VT100 escape decoder → KeyEvent, modifier parsing
  line.rs      — Line editing buffer: cursor, insert/delete, word ops, kill ring
  prompt.rs    — Prompt rendering: user@host, colored pwd, git branch, dirty indicator
  render.rs    — Composites prompt + line + completions + history pager → VT100 sequences
  parse.rs     — Single-pass tokenizer/parser → CommandLine (flat, no AST)
  expand.rs    — Tilde, $VAR, $(), glob expansion (*, ?, **)
  exec.rs      — fork/exec, pipe plumbing, redirections, process groups, PATH cache
  builtin.rs   — cd, exit, fg, set, unset, alias, l, c, w/which/type, echo, pwd, true, false
  ls.rs        — Native directory listing (l builtin): stat, permissions, color — like ls -plAhG
  history.rs   — Append-only file, in-memory Vec, prefix search, fuzzy subsequence search
  complete.rs  — File completion via readdir, grid layout (column-major), arrow navigation
  alias.rs     — AliasMap (HashMap wrapper), inline expansion on space
  config.rs    — Parse ~/.config/ish/config.ish (set + alias directives)
  job.rs       — Single suspended job (Ctrl+Z / fg)
  signal.rs    — Self-pipe pattern for SIGINT/SIGTSTP/SIGCHLD/SIGWINCH
  error.rs     — Shell error type (Io, Msg, GlobNoMatch, BadSubstitution)
```

### Data Flow

**Keypress to execution:**
1. `input.rs` reads raw bytes from stdin, decodes VT100 escapes into `KeyEvent`
2. `main.rs` dispatches the key to the current `Mode` (Normal / Completion / HistorySearch)
3. In Normal mode, printable chars go into `LineBuffer`; Enter triggers parse+exec
4. `parse.rs` tokenizes the line into a `CommandLine` (pipelines connected by `&&`/`||`/`;`)
5. `exec.rs` expands each word (tilde → vars → command subst → globs), then runs builtins in-process or fork/execs externals
6. `render.rs` repaints the prompt line after every keystroke using `TermWriter`

**Tab completion:**
1. Extract the word under cursor from `LineBuffer`
2. `complete::complete_path()` does readdir + prefix filter
3. Single match → auto-insert. Multiple → `compute_grid()` for column-major layout
4. Arrow keys navigate the grid; Enter accepts; Escape cancels

**History search (Ctrl+R):**
1. `history::fuzzy_search()` does subsequence matching across all entries
2. `render::render_history_pager()` shows results with highlighted match positions
3. Up/Down cycle through matches; Enter accepts; Escape cancels

## Key Data Structures

```rust
// parse.rs — Flat command representation, no AST
CommandLine { segments: Vec<(Pipeline, Option<Connector>)> }
Pipeline    { commands: Vec<PipedCommand> }
PipedCommand { cmd: Command, pipe_stderr: bool }  // pipe_stderr for &|
Command     { argv: Vec<String>, redirects: Vec<Redirect> }
Connector   = And | Or | Semi

// line.rs — Editing buffer
LineBuffer { text: String, cursor: usize, kill_ring: String }

// history.rs — Search results
FuzzyMatch { entry_idx: usize, score: i32, match_positions: Vec<usize> }

// complete.rs — Completion state
CompletionState { entries: Vec<CompEntry>, selected: usize, cols: usize, rows: usize, scroll: usize, dir_prefix: String }
CompEntry { name: String, is_dir: bool, is_link: bool, is_exec: bool }

// main.rs — Shell state (lives in main.rs, not the library)
Shell { aliases, last_status, prev_dir, rows, cols, history, prompt, job, path_cache, ... }
```

## Supported Syntax

**Pipes and chains:** `cmd1 | cmd2 && cmd3 || cmd4 ; cmd5`

**Pipe stderr:** `cmd1 &| cmd2` (fish-style, pipes both stdout+stderr)

**Redirections:** `>`, `>>`, `<`, `2>`, `&>` (stdout+stderr to file)

**Quoting:** `"double $VAR"`, `'single literal'`, `\"escaped`

**Expansion order:** tilde → `$VAR` → `$(cmd)` / `` `cmd` `` → glob (`*`, `?`, `**`)

**Continuation lines:** Incomplete input (trailing `|`, `&&`, `||`, unclosed quotes) prompts for more with `  ` (two-space indent).

## Builtins

| Builtin | Behavior |
|---------|----------|
| `cd [dir]` | Change directory. `cd -` returns to previous. Invalidates git cache + PATH cache. |
| `exit [code]` | Exit shell. Warns if suspended job exists. |
| `fg` | Resume single suspended job. |
| `set VAR val` | Set env var. No args lists all. `set PATH ...` rebuilds PATH cache. |
| `unset VAR` | Remove env var. |
| `alias name cmd args...` | Define alias. No args lists all. Single arg shows one. |
| `l [path]` | Native `ls -plAhG`: permissions, owner, group, human sizes, color, symlink targets. |
| `c` | Clear screen. |
| `w`/`which`/`type` | Show alias, builtin, or PATH location. |
| `echo` | Print arguments. |
| `pwd` | Print working directory. |
| `true`/`false` | Return 0 or 1. |
| `copy-scrollback` | Copy session transcript to clipboard via OSC 52. |

"Special builtins" (`cd`, `exit`, `fg`, `set`, `unset`, `alias`) modify shell state and run in the main process. "Output builtins" (`l`, `c`, `echo`, `pwd`, etc.) can be forked into pipelines.

## Line Editing Keybinds

| Key | Action |
|-----|--------|
| Ctrl+A / Home | Move to start |
| Ctrl+E / End | Move to end |
| Ctrl+W | Delete word backward |
| Ctrl+U | Delete to start |
| Ctrl+K | Delete to end |
| Ctrl+Y | Yank (paste kill ring) |
| Ctrl+L | Clear screen |
| Ctrl+R | Fuzzy history search |
| Ctrl+C | Cancel current line |
| Ctrl+D | Exit (empty line) or delete forward |
| Alt+B / Ctrl+Left | Move word left |
| Alt+F / Ctrl+Right | Move word right |
| Alt+D | Delete word forward |
| Up/Down | History navigation (prefix-filtered if text present) |
| Tab | File/directory completion |

## Building and Testing

```bash
cargo build              # Debug build
cargo build --release    # Optimized build (LTO, stripped)
cargo test               # All tests (unit + integration + pty)
cargo test --test pty    # PTY visual tests only
cargo test --test integration  # Library integration tests only
cargo bench              # Criterion benchmarks with allocation tracking
cargo +nightly fuzz run fuzz_parse  # Fuzz the parser (requires cargo-fuzz)
```

## Test Structure

### Unit tests (`cargo test --lib`)
26 tests embedded in source modules (input.rs, expand.rs, etc.) for isolated logic.

### Integration tests (`tests/integration.rs` — 205 tests)
Exercises the library API directly: parsing, expansion, line buffer, history, completion grid, aliases, config, prompt, builtins, ls. Achieves 95%+ coverage of all library modules.

Key testing patterns:
- Tests that mutate env vars are consolidated into single sequential tests to avoid parallel races
- All file-dependent tests use temp directories with absolute paths
- Prompt git tests assert on structure (`ends_with(" $ ")`) not exact branch names

### PTY tests (`tests/pty.rs` — 44 tests)
Spawns the real `ish` binary in a pseudo-terminal and drives it with keystrokes. Each test gets an isolated HOME directory with controlled files, history, and config. Tests the full shell loop: raw mode, prompt rendering, line editing, completion, history search, aliases, pipes, redirects, globs, exit handling.

The PTY harness (`PtyShell`) uses `openpty()` + `fork()` to create an isolated terminal session with a fixed 80x24 size. Assertions operate on visible terminal output with ANSI stripping.

### Fuzz targets (`fuzz/fuzz_targets/` — 5 targets)
- `fuzz_parse` — Parser never panics on any input
- `fuzz_expand` — Expander never panics (capped at 256 bytes)
- `fuzz_line` — LineBuffer invariants (cursor in bounds, at char boundary)
- `fuzz_config` — Config parsing functions never panic
- `fuzz_history` — Fuzzy match positions valid and ascending

### Benchmarks (`benches/bench.rs` — 14 groups)
Criterion benchmarks with a custom counting allocator that tracks heap allocations and bytes. Covers: parsing, expansion, line buffer ops, history search, completion grid, prompt rendering, end-to-end parse+expand, PATH cache, alias lookup, `ls` builtin, and filesystem completion. Includes an allocation audit section that prints per-operation allocation counts.

## Design Principles

**Simplicity over features.** Every feature earns its place through daily use. No configuration knobs, no plugin system, no themes. The shell does one thing: run commands interactively, with just enough UX to be pleasant.

**Native everything.** The shell never forks for its own operations. `l` does readdir+stat+getpwuid+getgrgid. Git branch reads `.git/HEAD` directly. Glob expansion walks the filesystem with readdir. This keeps the shell fast and self-contained.

**Flat data structures.** The parser produces a flat `CommandLine` with no recursive AST. No nesting means simple, predictable execution. The completion grid is a flat Vec with column-major indexing. History is a flat Vec with linear search.

**Minimal allocation.** Hot paths (prompt render, key dispatch, alias lookup) are profiled for allocation count. The benchmark suite tracks allocations per operation. Avoid unnecessary String clones; prefer `&str` where possible.

**Unsafe is contained.** All unsafe code is in 4 modules: `term.rs` (termios), `signal.rs` (signal handlers), `exec.rs` (fork/exec), and `input.rs` (raw fd reads). The rest of the codebase is safe Rust.

## Common Tasks for Agents

**Adding a builtin:** Add the name to `ALL_BUILTINS` (and `SPECIAL_BUILTINS` if it modifies shell state) in `builtin.rs`. Implement in `run_special()` or `run_output()`. If it needs shell state access beyond env vars, it must be special.

**Adding a keybind:** Handle it in `handle_normal_key()` in `main.rs`. The `KeyEvent` provides `.key` (enum) and `.mods` (ctrl/alt/shift). Return the appropriate `KeyAction`.

**Modifying the prompt:** Edit `prompt.rs`. The `render()` method builds the prompt string. Git branch detection is cached and invalidated on `cd`.

**Adding a parser feature:** Edit `parse.rs`. The tokenizer is a single-pass char scanner. New tokens go in the `Token` enum, new syntax in the parser loop. Keep it flat — no recursive descent.

**Running tests after changes:** Always run `cargo test` (all suites). The PTY tests (`--test pty`) catch rendering regressions that unit tests miss. Pre-commit hooks enforce `cargo fmt` and `cargo clippy -- -D warnings`.

## File Size Reference

| File | Lines |
|------|-------|
| main.rs | 884 |
| exec.rs | 574 |
| parse.rs | 446 |
| expand.rs | 402 |
| input.rs | 333 |
| prompt.rs | 295 |
| ls.rs | 288 |
| line.rs | 265 |
| render.rs | 247 |
| builtin.rs | 248 |
| complete.rs | 235 |
| history.rs | 196 |
| config.rs | 161 |
| term.rs | 158 |
| signal.rs | 88 |
| error.rs | 55 |
| alias.rs | 32 |
| lib.rs | 17 |
| job.rs | 4 |
| **Total src/** | **4,898** |
| tests/integration.rs | 2,453 |
| tests/pty.rs | 1,027 |
| benches/bench.rs | 832 |
| **Total** | **9,210** |
