# epsh Integration Notes

## Architecture

ish is a pure interactive layer on top of epsh. epsh provides parsing,
expansion, command evaluation, pipelines, and a broader general-purpose shell
runtime. ish intentionally exposes only its interactive subset and handles line
editing, completion, history, prompt, aliases, job control, and denv.

The main loop flow:
1. `read_line()` — interactive input with completion, history search, etc.
2. Intercept ish-level commands (alias, exit, cd, denv, fg, z, etc.)
3. Expand aliases (text substitution)
4. `epsh.run_script(&expanded)` — epsh does everything else
5. Observe state changes (cwd, exit status) and run hooks

## Exec Error Reporting

Because ish replaces `eval_external` with its own handler (issue #2 below), it
must reproduce epsh's exec error messages itself. Spawn failures in the handler
call `epsh::eval::bad_interpreter_message` (`src/main.rs`, `make_external_handler`)
so a script whose `#!` interpreter is broken reports
`cmd: interp: bad interpreter: <errno>` like the default exec path. The same
function is used by `eval::handle_exec_error` in epsh (`src/eval.rs`), which is
why the message format stays in sync.

## Known Issues

### 1. ~~Environment variable sync~~ (RESOLVED)

epsh's variable store is the single source of truth for the environment.
`set`/`export`/`unset` run as epsh builtins and only mutate the store. The
external handler (`make_external_handler`) builds the child environment
entirely from the store (`cmd.env_clear()` + the complete `env_for_command_os`
pair list the handler receives), so children see exactly the store, not ish's
process environment. denv's bash evaluator receives the same explicit pair
list; its temporary process-environment mirror exists only for denv's own
snapshot and restore bookkeeping. ish's own in-process PATH lookups (command coloring via
`PathCache`, `which`/`type` via `path::scan_path`, completion via
`path::complete_commands`) also take `$PATH` from the store, as do the
handler's `history` HOME lookup and the SSH-completion child. denv still
evaluates and restores its snapshot against that temporary process mirror.
`EnvChange`s returned by denv are applied to epsh vars via
`apply_denv_changes` in the main loop. The O(n) `sync_env_to_epsh`
reconciliation is gone.

### 2. External handler does fork/exec

**Problem:** The external handler reimplements fork/exec with job control
(setpgid, tcsetpgrp, WUNTRACED) for single commands. This duplicates what epsh
already does for pipelines. It exists because epsh's `eval_external` doesn't do
terminal handoff or stop detection for single commands in interactive mode.

**Why it's janky:** ~50 lines of fork/exec logic that mirrors epsh's pipeline code.
Also needs a `getpid() == shell_pid` check to distinguish main process from
pipeline children.

**Fix on ish side:** Not cleanly fixable. The handler must fork/exec because
single commands need tcsetpgrp/WUNTRACED and epsh doesn't provide that outside
of pipelines. Accepting this as the cost of keeping epsh minimal.

**Fix on epsh side (if ever):** Add tcsetpgrp/WUNTRACED to `eval_external` when
`interactive: true`. Then the handler only needs to handle ish builtins, and
external commands fall through to epsh.

### 3. ~~cd intercepted via text rewriting~~ (RESOLVED)

Simple `cd` commands are now intercepted in the main loop via
`change_directory()`. Handles
`cd -` (OLDPWD lookup), `cd` (HOME), and `cd path` directly. Sets OLDPWD/PWD in
both process env and epsh vars, triggers denv, updates dir stack and prompt.
Compound commands (`cd /tmp && ls`) still fall through to epsh with cwd detection.

### 4. Large main-loop interception list

**Problem:** The main loop matches on 10+ command names before falling through to
epsh: alias, exit, cd, history, copy-scrollback, denv, fg, z, w/which/type, l, c.

**Why it's janky:** Long match block. Most of these don't actually need main-loop
access — they just need shell state (job, history, aliases, session_log).

**Fix on ish side:** Move ish builtins into the external handler via shared state.
Wrap the mutable fields (job, history, aliases, session_log, denv_active, etc.) in
`Rc<RefCell<IshState>>`. The handler closure captures a clone of the Rc. Only
`alias` (modifies the alias map used by expand_line) and `exit` (needs to break
the loop) stay in the main loop. This cuts the match block from ~100 lines to ~20.

### 5. ~~Fragile cd detection for denv~~ (RESOLVED)

Gone — cd is intercepted in the main loop (see #3), so denv always triggers.

## Minimal epsh Changes Still Needed

These are genuinely blocking or high-value and can't be worked around on ish side:

1. **Handler returns `Option`** — Change `ExternalHandler` to return
   `Option<Result<ExitStatus>>`. `None` means "not handled, fall through to
   eval_external." This lets the handler only handle ish builtins instead of
   reimplementing fork/exec. Depends on #2 in epsh below.

2. **Interactive `eval_external`** — Add tcsetpgrp/WUNTRACED to `eval_external`
   when `interactive: true`, matching what `eval_pipeline` already does. Without
   this, single commands like `vim` can't get the terminal or be suspended.

These two changes together eliminate the fork/exec duplication in ish's handler
(issue #2 above). They're the only epsh changes that would meaningfully improve
the integration.

## Remaining Next Steps (ish side only)

1. **Shared state for external handler** — fixes #4, no epsh changes. Wrap mutable
   shell state in `Rc<RefCell<IshState>>` so the external handler can dispatch
   builtins like `fg`, `z`, `w` directly. Cuts the main loop match block.
