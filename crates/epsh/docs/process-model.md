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

An embedder can register a raw command name with
`Shell::set_command_placement(name, CommandPlacement::ParentProcess)`. The
placement table owns each name's bytes and is empty by default. epsh resolves
nested `command` indirection after expansion, then checks placement before
calling a command policy, opening the target's redirections, or invoking a
builtin, function, or external handler. A registered target is refused with
status 2 when its evaluator process differs from the Shell's owner or when it
runs in a logical pipeline or command substitution. This also covers evaluator
paths that continue in the current process. Alias expansion belongs to the
embedding shell; the expanded command name is checked by the same rule.

A one-command negated pipeline such as `! cp` does not create a pipeline child,
so a parent-process command can run in the Shell's owner process. epsh then
applies the usual status inversion. A refusal inside a real pipeline or command
substitution still writes its diagnostic and stops before target effects. The
refusal returns status 2, and `!` applies its usual inversion to that status.
Cancellation and timeout propagate as control-flow errors. Negation does not
invert them or continue the list.

An embedder can register command lookup identity separately with
`Shell::set_command_identity(name, CommandIdentity::Builtin)`. The identity
table owns the name bytes and is empty by default. `command -v`, `command -V`,
and `type` report registered names as shell builtins. Identity does not affect
dispatch or process placement. Names containing `/` are resolved as paths and
do not use identity metadata.

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
buffer duplication. A flush error aborts setup. Pipeline setup errors terminate
and reap only children started by that pipeline. Earlier background children
remain waitable and keep their output relays. Cleanup terminates a dedicated
pipeline process group, or individual pipeline children when they share the
evaluator's process group.

Safe callback registrations record their registering process. Every invocation
checks the real process identity, including forks made outside epsh.
External handlers, name and argv policies, and writable redirection openers
return `ShellError::CallbackProcess` when inherited by another process.
Dispatch preflights required callbacks before target redirections and checks
again immediately before invocation. A refused opener never falls back to a
default open. Inherited disallowed callbacks are neither invoked nor dropped.

Unsafe `set_*_in_children` registrations require an audit of all reachable
operations, destructors, and error paths at every possible fork point.
Inherited host locks, runtime services, environment mutation, and dependencies
on vanished threads violate that contract. Immutable captures and `Send` do
not prove safety. Child mutations remain local to the child.

Each evaluator fork has a private close-on-exec control pipe. Callback and
command-policy refusals remain typed across waits and command substitutions.
A refusal during substitution suppresses the enclosing target's effects.
Numeric entry points report status 2. Ordinary exit 2 does not use this channel
and preserves ordinary substitution output. Earlier commands and completed
expansion effects are not rolled back.

These checks establish the callback boundary. The Rust evaluator still runs
after fork and has no general safety guarantee after a multithreaded fork.

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

`run_program` and `run_script` store their returned status in `exit_status()`,
which supplies `$?` on the next call. Explicit exit stores the requested status.
Cancellation and timeout store 130. Stopped execution stores 148. Parse and runtime
errors store 2. EXIT traps run on normal completion, explicit exit, and runtime
errors. They see the terminal status through `$?`. Trap commands do not replace
the returned or stored terminal status. Parse errors, cancellation, timeout, and
stopped execution return without running EXIT traps.

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
