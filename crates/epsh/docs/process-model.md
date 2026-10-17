# Process model

epsh keeps a working directory on each Shell instance. The cd builtin updates
that shell state without changing the embedder's process working directory.
Default external commands run in the shell directory. An external handler can
query the directory for its current call with external_command_cwd().
Relative file predicates in `test` also use the shell directory.

## External commands

Without an external handler, ordinary commands run through
std::process::Command. In an already-forked pipeline child, epsh can replace
that child directly with the external program. The exec builtin also replaces
the current process when given a command.

With an external handler, epsh passes expanded arguments and the complete child
environment as raw byte pairs. Redirections have already been applied to file
descriptors. The handler owns process creation and waiting; it should use
external_command_cwd() when it needs the shell's working directory. The
environment includes inherited entries unless env_clear was selected, exported
shell variables, and command-prefix assignments.

An embedder can install a command policy on the builder or with
`Shell::set_command_policy`. epsh calls it with the expanded target before
builtin or function dispatch. The policy also sees targets reached through
nested `command` builtins and command substitutions. Returning a diagnostic
stops the target with status 2 before its effects and redirections. The default
shell has no policy. A policy does not undo effects from earlier commands or
expansion.

The optional redirect-open handler owns only the opening of writable targets:
truncating output, forced output, append output, and read/write redirections.
It must honor the requested create, truncate, append, or read-write mode. Input
redirections, descriptor duplication, and here-documents continue through epsh.

## Forks, waits, and cancellation

epsh uses libc fork for pipelines, subshells, background commands, and command
substitutions. A forked child may continue running the Rust evaluator, including
builtins and expansion, rather than immediately calling exec. Replacing this
with a raw fork primitive that requires raw operations until exec would change
that execution model.

Before an evaluator fork, epsh locks and flushes Rust stdout, then releases the
copied guard in both processes. This prevents inherited stdout locks and parent
buffer duplication. A flush error aborts setup. The current pipeline error path
terminates all tracked children; its cleanup scope is tracked in
[ISH-13](../../../TODO.md#ish-13--p2--pipeline-setup-errors-terminate-unrelated-background-children).

Embedding callbacks can execute in forked evaluator children. A captured host
mutex held by another thread at fork can remain locked in the child after the
parent unlocks it. The stdout guard does not protect arbitrary callback state.
The pending boundary decision is tracked in
[ISH-8](../../../docs/rfc/ISH-8-callback-process-boundary.md).

The default executor checks cancellation and deadlines between commands and
while waiting for children. On cancellation or timeout it terminates tracked
child process groups. A custom external handler owns the lifecycle of the
processes it starts.

The `wait` builtin waits only for children started by its Shell. A PID operand
selects that child's exit status. Command substitutions isolate variable and
directory changes; a nonzero command status still produces substitution output.

Signal traps run between commands. They are not dispatched while the shell is
blocked in a child wait.
Trap names are case-insensitive and accept a `SIG` prefix. Installing a trap for
an uncatchable signal such as KILL fails without changing the registered traps.

## Interactive integrations

Interactive mode enables foreground terminal handoff and stopped-child
detection for pipelines. epsh does not implement prompts, line editing, job
tables, or fg/bg/jobs builtins.

At the lower level, eval_command can return ShellError::Stopped with the
stopped PID and pipeline process-group ID. run_program converts that error to
exit status 148, so callers that need the IDs must handle commands through
eval_command. This distinction is part of the current implementation.

See [eval.rs](../src/eval.rs) for execution and wait paths and [sys.rs](../src/sys.rs)
for the syscall wrappers.
