# ISH-15, ISH-52 and ISH-48: Environment and shell ownership

Status: accepted. ISH-15, ISH-52 and ISH-48 implemented with focused Linux
acceptance (2026-10-22); native macOS was not exercised.

## Decision

Remove process-environment mutation from ish, including its public library
interface. Keep the interactive `Shell` private to the binary in `src/shell.rs`;
extract editor state and event handling into the library's `src/editor.rs`.
The environment ownership change and module extraction are implemented.

## ISH-15: One environment owner

epsh's variable store is the source of shell variables and exported child
environments. Import the inherited environment once when constructing the
interactive shell, then set `SHELL`, `PWD` and `OLDPWD` in that store. Delete
`shell_setenv`, `shell_setenv_os` and `shell_unsetenv`; do not retain safe wrappers
or replace them with an unsafe public compatibility API. Startup and directory
changes must not mirror values into the host process environment either.

This removes a precondition ish cannot enforce for library callers.
[Rust's `set_var` safety contract](https://doc.rust-lang.org/std/env/fn.set_var.html#safety)
requires excluding concurrent environment reads outside Rust's environment
module as well as writes on Unix; an ish mutex or joining ish's own workers
does not establish that condition for an arbitrary host.

### denv implementation

`denv` stays in the library, with explicit inputs and results:

1. Read cwd from the supplied epsh shell's cwd and build an owned working
   environment from its exported byte-preserving values. Do not use a mutable
   `PWD` variable as the authority for filesystem traversal or fall back to the
   host cwd inside denv.
2. Apply restoration of the previous denv layer to that working environment.
   Compute trust state and dotenv differences against it. For affected names,
   save the previous shell binding and its export flag from the supplied store,
   so a previously unexported local is not lost or exported on restoration.
   Replace the ambient reads in `state_var_matches`, `diff_dotenv_only` and
   `capture_prev`, along with `capture_process_env` and `restore_process_env`.
3. Seed the epsh evaluator from the explicit working environment without
   temporarily changing the process environment. Add a narrow epsh constructor
   or builder input for an explicit environment, sharing the import rules used
   by `Variables::new`; preserve inherited entries that are not shell variable
   names. `.env_clear()` plus UTF-8-only `set_var` calls is insufficient.
4. Keep the bash evaluator and every external spawn on explicit
   [`Command::env_clear` plus `envs`](https://doc.rust-lang.org/std/process/struct.Command.html#method.env_clear).
   `PATH`, `HOME`, `TMPDIR`, cwd and command-prefix assignments must come from
   the shell context. Audit direct command creation and in-process readers so
   removal of the ambient mirror cannot silently use stale values.
5. Make `set_change` and `unset_change` build results without side effects.
   Validate a complete delta, including readonly-variable errors, before
   committing it to the caller's variable store; stop discarding assignment
   errors in `apply_denv_changes`. A failed evaluation contributes none of its
   assignments. Restoring the previous directory's layer and recording the
   existing blocked/dirty state are deliberate transitions, not partial results
   from the failed script. Persist the matching active record before publishing
   a successful new layer; failed persistence must not install that layer.

Keep raw inherited names and values byte-preserving at the environment boundary.
Saved prior bindings must restore exactly, including non-UTF-8 bytes, export
flags and the difference between unset and empty. Use a versioned active-record
encoding if needed, with an explicit reader/migration for existing records; do not silently
reinterpret their format or replace bytes with lossy display text. `.env` and
`.envrc` source syntax need not become a new byte-oriented language.

Process cwd, signals and terminal ownership are separate concerns. This decision
does not promise arbitrary concurrent execution of an interactive shell or fix
the evaluator's [callback fork boundary](ISH-8-callback-process-boundary.md).

## ISH-52: Remove the unsupported safety claims

Close ISH-52 with the ISH-15 implementation, after deleting both environment
mutation blocks in `restore_process_env` and their single-threaded comments.
Do not replace those comments with a guessed worker-lifetime argument. Audit
remaining production code for `std::env::{set_var, remove_var}`, libc equivalents
and wrapper aliases. Any remaining unrelated unsafe boundary must describe its
own actual preconditions. Test fixtures should supply environments through
`Command::env` or explicit stores instead of mutating the test runner's globals.

## ISH-48: Private session, testable editor

`src/shell.rs` is a **binary module**, declared by `main.rs`, not exported by
`lib.rs`. Its private `Shell` owns the epsh evaluator, denv session, history,
aliases, current job, output capture, input reader and runtime lifecycle.
`main.rs` handles CLI arguments and enters that runtime. Shell-state command
routing moves with `Shell`; update the corresponding AGENTS topic/invariant
when implementing the relocation.

`src/editor.rs` belongs to the library. It owns the line, mode, selection,
history-navigation and completion scratch state now spread across `EditView`,
mode handlers and `Shell`'s editor buffers. Expose only the concrete editor
entry points and types the binary needs, with private fields and private
per-mode handlers. Use ordinary borrowed context for aliases, history, cwd,
shell-derived paths and prompt data; do not pass the whole interactive `Shell`
through a public trait or introduce a second variable store.

Key/paste/resize handling returns typed actions such as submit, cancel, exit,
request completion and redraw. The binary performs terminal polling, raw-mode
changes, output flushing and command execution. Rendering can fill the existing
reused output buffer, but must be testable without writing to the real terminal.
Resize supplies measured dimensions; editor code does not call terminal ioctls.
Completion, history and directory enumeration remain in-process. This is a
module extraction with a narrow testing seam, not a plugin framework or a new
supported interactive embedding API; epsh remains the embedding layer.

Preserve reusable buffers and current keystroke behavior. No per-event heap
allocation is required for dispatch or action tagging. Keep the one-suspended-job
rule and the [ISH-5 ownership decision](ISH-5-foreground-job-outcomes.md) in the
runtime, outside editor state.

Fix `diagnostics::write_layout_dump_file` in the same ticket: retry only
`AlreadyExists`; return other open errors with their real path and cause.
Reserve the unique-filename-exhaustion message for exhausted collision retries.

## Implementation order and acceptance

ISH-15 and ISH-52 form one environment change. ISH-48 can extract editor handlers
independently, but its final integration must use the explicit environment
boundary above. It does not need to wait for ISH-5 or ISH-8 implementation;
preserve their ownership boundaries while moving code.

- Run two denv sessions with different explicit environments and cwd values;
  loading, reloading, denial, leaving a directory and failure must not leak into
  each other or the host. Exercise the dotenv-only public path while an unrelated
  thread reads an inherited sentinel, with all setup supplied by a subprocess's
  environment. This tests environment isolation, not post-fork safety.
- Cover epsh and bash evaluation, prefix assignments, exported/unexported
  variables, empty/unset values, readonly failures, non-UTF-8 inherited values,
  exact previous-value restoration, and failing active-record persistence.
  Assert rejected evaluations install none of their assignments. Check children
  observe the selected shell environment and the host environment stays intact.
- Retain PTY coverage for exports, directory changes, denv, command lookup,
  completion and prompt identity; changing `HOME`, `PATH` or `TMPDIR` in the
  shell must affect the intended consumer without mutating the host environment.
- Unit-test normal, completion, history-search and directory-picker handlers
  with synthetic events and a memory output sink: cancellation restores the
  saved line, acceptance submits the selected text, paste respects the active
  mode, and resize clamps selection and layout. Retain PTY coverage for actual
  terminal restoration and command execution.
- Exercise dump-file collision retries and a non-collision open failure; assert
  the latter retains its actual error. Run the existing workspace checks and
  relevant PTY suites after implementation. This documentation commit itself
  changes no runtime behavior and does not close any of these three tickets.
