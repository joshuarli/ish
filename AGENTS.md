ish is a small, interactive-only shell written in Rust. It prioritizes a fast
native prompt, line editing, history search, completion, aliases, and minimal
job control. It is intentionally not a general-purpose scripting shell.

The important design constraints are:

- Interactive use only. Do not add scripting, POSIX-compatibility, `source`,
  `eval`, control flow, functions, background jobs, plugins, themes, or prompt
  configuration without an explicit design decision.
- Shell-owned operations stay in-process. Directory listing, completion,
  finder searches, globbing, prompt/git inspection, and similar operations must
  not spawn a subprocess. Running a user command or the existing denv
  integration may spawn one.
- Keep the execution model and data structures simple and flat. Avoid adding
  an ish-owned AST, a general configuration language, or abstraction layers
  that do not pay for themselves.
- Preserve the interactive hot path. Reuse buffers where practical and avoid
  allocations in code that runs on every keystroke; measure before making a
  performance claim.

## Dependencies and platform code

Prefer `rustix` to the greatest extent possible for Unix/system calls and file
descriptor, process, terminal, signal-adjacent, and time operations. Use
`libc` only when rustix does not expose the required API or when a measured,
allocation-sensitive libc interface is deliberately retained. Keep such
exceptions narrow and document the reason in the code. Do not add another
crate without checking whether the standard library or an existing dependency
already solves the problem.

Unsafe code should remain localized to low-level platform boundaries. Prefer
safe rustix APIs and RAII-owned descriptors; audit any new unsafe code for
ownership, aliasing, error handling, and platform differences.

`epsh` is used for config/alias parsing and expansion. Do not accidentally
turn it into a general shell execution path or expand ish's supported syntax
without an explicit decision.

## Architecture

The main flow is:

```text
input → line editing → alias handling → command dispatch → render
                                      ├→ ish builtin
                                      └→ epsh parse/expand/evaluate → job or external command
```

Subsystem ownership:

- `main.rs`: shell state, event loop, modes, and key dispatch.
- `input.rs`, `line.rs`: terminal byte decoding and UTF-8 line editing.
- `render.rs`, `prompt.rs`, `term.rs`: terminal output, prompt, raw mode, and
  repainting.
- `builtin.rs`, `job.rs`, `sys.rs`, `signal.rs`: builtins, foreground job
  control, process/file-descriptor plumbing, and signal handling.
- `complete.rs`, `finder.rs`, `path.rs`, `ls.rs`: native completion, file
  finding, PATH lookup, and directory listing.
- `history.rs`, `frecency.rs`: persistent history, fuzzy search, and ranking.
- `alias.rs`, `config.rs`, `denv.rs`: aliases, `config.ish`, and denv
  integration.

Read the relevant module and its tests before changing behavior. Keep public
interfaces narrow and preserve useful comments, especially comments explaining
platform workarounds, safety, allocation behavior, or non-obvious constants.

## User-visible behavior

Supported shell behavior includes pipelines, AND-OR lists (`&&`/`||`),
sequential lists (`;`), redirections, quoting and escaping, comments,
continuation lines, tilde expansion, parameter expansion, command substitution,
pathname expansion (globbing), aliases, history search, file completion, the
native file finder, and one suspended foreground job.

Builtins and keybindings are documented in [README.md](README.md). Treat the
README and existing tests as the behavior contract; update both when a
user-visible behavior intentionally changes. Avoid documenting implementation
details here that are likely to change.

Notable builtin rules:

- State-changing builtins (`cd`, `exit`, `fg`, `set`, `unset`, and `alias`) must
  run in the shell process.
- Pipeline-safe output builtins may participate in pipelines.
- New builtins belong in the builtin registry and in the appropriate
  state-changing or pipeline-safe execution path, with integration/PTY coverage
  as applicable.

## Shell terminology

Use these terms consistently in code, comments, tests, and documentation:

- An input line is raw text from the line editor.
- A token is a lexical unit, including operators such as `|`, `&&`, and `>`.
- A shell word is an unexpanded syntactic word; `epsh::ast::Word` contains its
  word parts.
- A word part is literal text, quoting, parameter expansion, command
  substitution, or tilde expansion.
- A field is an argument produced after expansion, suitable for `argv`.
- A simple command contains assignments, a command name, arguments, and
  redirections.
- A pipeline joins commands with `|` or `|&`.
- An AND-OR list joins commands with `&&` or `||`.
- A sequential list joins commands with `;` or a newline. Use list as the
  umbrella term; do not call every composition a pipeline or a chain.
- A redirection routes a file descriptor, such as `>file`, `2>file`, or `&>file`.
- Parameter expansion handles `$HOME` and `$PATH`; pathname expansion, also
  called globbing, handles patterns such as `*.rs`.
- A builtin is implemented by the shell. An external command is launched as a
  child process. A job is the process group and terminal state for a foreground
  command or pipeline.

The `epsh` AST supports more shell constructs than `ish` intentionally exposes.
Do not describe parser capabilities as supported `ish` behavior without checking
the interactive dispatch and README contract.

## Testing

`cargo test` includes library, integration, and PTY tests. PTY tests exercise
the real binary and are especially important for terminal rendering, signals,
completion, history, aliases, pipelines, redirections, and exit behavior.
Run focused tests while iterating, then run the full suite before handoff.
Benchmarks and fuzz targets are useful when changing hot paths, parsers,
expansion, or safety-sensitive filesystem/process code.

## Coding style

Prefer the simplest correct implementation. Avoid premature abstraction and
unnecessary dependencies. Never add banner/separator comments. Do not remove
useful comments during refactors; update them when the behavior changes.

Before editing, inspect the worktree and preserve unrelated user changes. Keep
changes scoped to the request. For behavior changes, add or update the smallest
test that locks in the intended result; use temporary directories and isolated
environment state in filesystem/environment tests.

Do not run pre-commit hooks. Do not push to a remote. Leave commits and
verification of commits to the user.

## Common changes

- Builtin: start at `src/builtin.rs` (`ISH_EXTENSION_BUILTINS`,
  `is_ish_extension_builtin()`, `is_builtin()`, `all_builtin_names()`,
  `locate_command()`, `list_directory()`). Commands that must change shell
  state are intercepted by the `first_command_word` match in `src/main.rs`;
  relevant handlers include `handle_alias()`, `handle_exit_command()`,
  `change_directory()`, `handle_history()`, `denv::command()`, and
  `job::resume_job()`. Add behavior tests in `tests/integration.rs` or
  `tests/pty.rs`.
- Keybinding or mode behavior: update `handle_normal_key()`,
  `handle_completion_key()`, `handle_history_search_key()`, or
  `handle_file_finder_key()` in `src/main.rs`. The mode state is the `Mode`
  enum; terminal actions are represented by `KeyAction`, `CompletionAction`,
  `HistoryAction`, and `FileFinderAction`.
- Completion: use `complete::complete_path_into()`,
  `complete::complete_candidates()`, `complete::CompletionState`, and
  `complete::compute_grid()` in `src/complete.rs`; orchestration and accept/
  refilter behavior live in `start_completion()` and
  `handle_completion_key()` in `src/main.rs`.
- History search: use `History::fuzzy_search_into()` or
  `History::fuzzy_search_subset_into()` and `FuzzyMatch` in `src/history.rs`;
  input and selection are handled by `handle_history_search_key()` and
  `render_history_mode()` in `src/main.rs`.
- Prompt or rendering: prompt data and git inspection are in `Prompt` and
  `shorten_pwd()` in `src/prompt.rs`; terminal composition is in
  `render_line()`, `render_completions()`, `render_history_pager_cached()`,
  and `render_file_finder()` in `src/render.rs`. Run PTY tests for changes
  visible on screen.
- Config or aliases: `config::load()`, `parse_set()`, and `parse_alias()` are
  in `src/config.rs`; `AliasMap` and `lex_words()` are in `src/alias.rs`.
  Keep expansion through the existing `epsh` integration.
- Native filesystem or syscall code: prefer rustix, keep unsafe at the
  boundary, document unavoidable libc use, and test error paths. Start with
  `src/sys.rs` (`pipe_cloexec()`, `close_fds_from()`,
  `spawn_command_subst()`), `src/path.rs` (`PathCache`, `scan_path()`),
  `src/ls.rs` (`list_dir()`), or `src/finder.rs` (`find_async()`) as
  appropriate.
- Parser/expansion behavior: inspect the call sites of
  `expand_builtin_args()` and the `epsh::lexer`/`epsh::expand` integration in
  `src/main.rs` and `src/alias.rs`. Distinguish shell words from expanded
  fields, and extend that integration rather than introducing a parallel parser.
