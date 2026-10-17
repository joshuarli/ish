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

Evaluator fork, wait, callback, and cancellation behavior belongs to the
[epsh process model](../crates/epsh/docs/process-model.md).

Module entry points are [src/lib.rs](../src/lib.rs) and the interactive event
loop in [src/main.rs](../src/main.rs). Parser and evaluator internals belong to
[epsh's architecture](../crates/epsh/docs/architecture.md).

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
each bridge family. It also compiles four isolated production mutants and
requires the bridge to detect their arithmetic, splitting, policy, and
descriptor regressions.

The separate Linux [`make verify-refinement` check](verification/refinement.md)
extracts the production shift-count helper through Charon and Aeneas. Lean proves
that the translated helper accepts exactly counts 0 through 63 and converts them
losslessly. The proof connects that result to both model shift branches. Its
trust boundary includes the extractor and its separate compiler version.
See [epsh testing](../crates/epsh/docs/testing.md) for Rust and native corpus commands.

Shell terminology follows [epsh's architecture](../crates/epsh/docs/architecture.md).
