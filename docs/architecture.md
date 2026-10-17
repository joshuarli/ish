# Architecture

ish is an interactive-only shell built around epsh. The implementation keeps
ish's user-facing syntax smaller than epsh's parser and runtime.

## Execution flow

1. The line editor returns an input line; aliases are expanded by the existing
   alias integration.
2. [`dispatch::plan`](../src/dispatch.rs) parses the line once with epsh and
   rejects constructs outside the supported subset. The accepted syntax is
   listed in the [README](../README.md).
3. A whole-line shell builtin is dispatched in ish. Other accepted programs are
   evaluated by epsh with ish's command policy and external-command handler.
4. The handler runs ish-owned builtins where needed and starts external
   commands with process-group and terminal control. ish synchronizes the
   resulting status and working directory.

The `Exec` class in [`builtin.rs`](../src/builtin.rs) defines where ish-owned
commands run. `InShell` commands must be the whole input line. `Hybrid`
commands such as `cd` and `history` run with shell state when alone and use the
stateless command path inside lists. `Native` commands can run in pipeline
stages. The registry is the source for builtin-name lookup and completion.

`epsh` supports more syntax than ish exposes. Keep the acceptance check in
`dispatch::plan()`; do not send unsupported syntax directly to epsh. Do not
add an ish-owned parser or general configuration language.

The epsh command policy checks expanded targets before builtin or function
dispatch. ish uses it to refuse dynamically selected `eval`, `source`, `.`, and
shell-state builtins outside their whole-line route. These refusals return
status 2 before the target runs or opens its redirections. The policy does not
undo effects from earlier commands or expansion.

## State and process boundaries

- Shell variables live in epsh's variable store. Child processes receive the
  exported values from that store; in-process command lookup and completion
  use its `PATH` value.
- A standalone `cd` runs in ish. For `cd` inside a list, epsh updates the
  command working directory; ish notices the resulting directory change and
  runs the directory-change hooks, including denv.
- Directory listing, completion, pathname expansion, and prompt Git inspection
  run in-process. External commands and command substitutions may start child
  processes. A `.envrc` that requests bash evaluation may also start one.

## Subsystem ownership

| Area | Owner |
|---|---|
| Event loop, shell state, mode and key dispatch | [`src/main.rs`](../src/main.rs) |
| Input decoding and editable text | [`src/input.rs`](../src/input.rs), [`src/line.rs`](../src/line.rs) |
| Prompt, rendering and terminal mode | [`src/prompt.rs`](../src/prompt.rs), [`src/render.rs`](../src/render.rs), [`src/term.rs`](../src/term.rs) |
| Parsing gate and command routing | [`src/dispatch.rs`](../src/dispatch.rs), [`src/builtin.rs`](../src/builtin.rs) |
| Foreground job control and system calls | [`src/job.rs`](../src/job.rs), [`src/sys.rs`](../src/sys.rs), [`src/signal.rs`](../src/signal.rs) |
| Completion and directory listing | [`src/complete.rs`](../src/complete.rs), [`src/path.rs`](../src/path.rs), [`src/ls.rs`](../src/ls.rs) |
| History search and ranking | [`src/history.rs`](../src/history.rs), [`src/frecency.rs`](../src/frecency.rs) |
| Config, aliases and denv | [`src/config.rs`](../src/config.rs), [`src/alias.rs`](../src/alias.rs), [`src/denv.rs`](../src/denv.rs) |

## Semantic verification

The Lean project in [`formal/`](../formal/) models arithmetic, expansion,
syntax policy, evaluation, effects, and interactive behavior. Its theorems
establish properties of those definitions. They do not prove that Rust
implements every modeled rule.

The development bridge parses inputs with epsh and runs admitted cases through
Rust entry points and Lean models. It compares their outcomes for six families:
policy, expansion, arithmetic, evaluation, effects, and interactive behavior.
Each family has an explicit input domain. Unsupported inputs and exhausted fuel
do not count as successes. Rust unit, integration, and PTY tests separately
check implementation behavior, including cases outside the bridge domains.
Process and PTY tests also exercise operating-system behavior.

The [`make verify` target](../Makefile) builds the formal project, checks
protocol fixtures, audits theorem axioms, and runs 1,000 generated cases for
each bridge family.
See [epsh testing](../crates/epsh/docs/testing.md) for Rust and native corpus commands.

## Shell terms

Use these terms consistently:

- An **input line** is raw text from the line editor.
- A **token** is a lexical unit, including operators such as `|`, `&&` and `>`.
- A **shell word** is unexpanded syntax; an epsh `Word` contains its word parts.
- A **field** is an argument produced by expansion for `argv`.
- A **simple command** has assignments, a command name, arguments and
  redirections.
- A **pipeline** joins commands with `|` or `|&`.
- An **AND-OR list** joins commands with `&&` or `||`; a **sequential list**
  joins commands with `;` or a newline. Use *list* for the general case.
- A **redirection** routes a file descriptor, as in `>file` or `2>file`.
- **Parameter expansion** handles `$HOME`; **pathname expansion** (globbing)
  handles patterns such as `*.rs`.
- A **builtin** is implemented by the shell; an **external command** runs in a
  child process. A **job** is the foreground process group and terminal state.
