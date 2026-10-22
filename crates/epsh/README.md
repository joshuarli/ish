# epsh

epsh is a non-interactive POSIX shell library and command-line runner for Unix.
Rust applications can run command strings and scripts, capture output, inspect
parsed commands before execution, and provide their own external-command
handler.

## Install

Install Rust and Cargo, then add epsh to a Rust project:

    cargo add epsh

To install the command-line runner:

    cargo install epsh

The crate has no dependency on ish and remains independently packageable. The
default installation includes only the `epsh` runner; test fixtures are reserved
for the crate's tests.
Workspace contributor commands are in [Testing](docs/testing.md).

## Use as a library

This example captures output as bytes, so it also works when command output is
not valid UTF-8:

    use epsh::eval::Shell;
    use std::{path::PathBuf, sync::{Arc, Mutex}};

    fn main() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cwd(PathBuf::from("/project"))
            .stdout_sink(stdout.clone())
            .build();

        let status = shell.run_script("printf 'hello\\n'");
        assert_eq!(status, 0);
        assert_eq!(stdout.lock().unwrap().as_slice(), b"hello\n");
    }

To parse once and inspect the AST before execution, parse the source and pass
the program to Shell::run_program:

    use epsh::{eval::Shell, parser::Parser};

    fn main() -> Result<(), epsh::error::ShellError> {
        let mut shell = Shell::new();
        let mut parser = Parser::new("echo hello");
        let program = parser.parse()?;
        let status = shell.run_program(&program);
        println!("{}", status.code());
        Ok(())
    }

Shell::run_script returns an integer exit code. Shell::run_program returns an
ExitStatus. By default, output goes to the process file descriptors. The sinks
capture builtin output and output relayed by epsh's default child executor.
Embedders that execute commands themselves can call
`Shell::set_exit_status(ExitStatus)` to set the next `$?` expansion.

`ExitStatus` constructors normalize integer codes to 0–255 modulo 256.
The `exit` and `return` builtins accept signed 32-bit integer operands and use
the previous status when the operand is omitted. Invalid or out-of-range
operands print a diagnostic and terminate the shell with status 2.

Shell values preserve arbitrary bytes; operations at OS and C-string
boundaries may reject NUL. Use set_var_bytes and get_var_bytes when Rust code
needs raw variable values; get_var returns a string only when the value is
valid UTF-8. See [byte preservation](docs/byte-preservation.md).

`Variables::bindings_bytes()` returns an owned snapshot of defined variable
names and raw values, sorted by name. It includes local and exported values,
omits unset placeholders, and does not expose variable flags or storage. Later
mutations to the variable store do not change the returned snapshot.

Use `parser::try_parse_word_parts` or `try_parse_word_parts_bytes` to receive
nested syntax errors. The existing `Vec`-returning word helpers retain their
legacy empty-command recovery; shell execution uses the strict parser.

## Builder options

| Method | Effect |
|--------|--------|
| cwd(PathBuf) | Set this shell's working directory; defaults to the process working directory at build time. |
| errexit(bool) | Enable set -e behavior. |
| nounset(bool) | Enable set -u behavior. |
| xtrace(bool) | Enable set -x tracing. |
| pipefail(bool) | Use the last nonzero pipeline status. |
| noglob(bool) | Disable pathname expansion. |
| noexec(bool) | Parse commands without executing them. |
| stdout_sink(...) / stderr_sink(...) | Capture builtin output and child output relayed by the default executor. |
| cancel_flag(Arc<AtomicBool>) | Stop execution when the flag becomes true. |
| timeout(Duration) | Set a deadline measured from shell construction. |
| env_clear() | Start without importing the process environment. |
| environment(entries) | Import owned OS name/value pairs instead, preserving raw bytes and non-shell names. |
| external_handler(...) | Delegate external command execution to the embedder. |
| redirect_open_handler(...) | Let the embedder open writable redirection targets. |
| interactive(bool) | Enable foreground terminal handoff and stopped-process detection for pipelines. |
| command_policy(...) | Refuse expanded command targets before builtin or function dispatch. |
| command_argv_policy(...) | Validate expanded raw argv before target redirections or dispatch assignments. |

Command policy is opt-in and disabled by default. The callback receives the
expanded target as `ShellBytes`. Returning a diagnostic refuses the target with
status 2. The policy also checks nested `command` indirection and command
substitutions. Rejection happens before the target runs or opens redirections.
The policy does not undo earlier commands or expansion effects.

Safe callback registrations execute only in the registering process. Child
invocation returns `ShellError::CallbackProcess` and maps to status 2 at numeric
entry points. This rule covers external handlers, both command policies, and
writable redirection openers. A private control channel preserves refusals
through evaluator forks and command substitutions. Refusal during substitution
suppresses the enclosing target. An ordinary command exiting 2 keeps normal
substitution semantics.

The `set_external_handler_in_children`, `set_external_dispatcher_in_children`,
`set_command_policy_in_children`, `set_command_argv_policy_in_children`, and
`set_redirect_open_handler_in_children` methods are unsafe opt-ins. Their safety
contracts cover every reachable operation, destructor, and error path.
Immutable captures and `Send` alone do not establish child safety.

`set_external_dispatcher(names, dispatcher)` selects a callback by owned raw
command names. It returns `ExternalDispatch::Handled(status)` or `NotHandled`.
The latter uses epsh's default executor with the already-expanded environment.
Status-only external handlers remain supported and own processes they create.

Interactive embedders use `run_program_outcome` and match `ProgramOutcome`.
`Completed` carries an exit status. `Stopped` transfers a non-cloneable
`ForegroundJob` containing the owned group and output relays. Call
`resume_foreground(&mut job)` until it returns `Some(status)` or use
`terminate_foreground(&mut job)`. `None` means the group stopped again.
The job belongs to its original shell and process. Dropping a job terminates
and reaps its owned children. Numeric `run_program` and `run_script` terminate
stopped jobs, print a diagnostic, and return 148.

Command placement is opt-in and disabled by default. Call
`Shell::set_command_placement` with an owned `ShellBytes` name and
`CommandPlacement::ParentProcess` to require that target to run in the process
that created the Shell. epsh resolves nested `command` execution after argument
expansion and checks placement before command-policy callbacks, target
redirections, builtins, functions, or the external handler. A registered target
is refused with status 2 in a pipeline, command substitution, or another
forked evaluator context. The table owns the command-name bytes. Existing
embedders keep their current behavior until they register a placement.

Command identity is opt-in and independent of placement. Call
`Shell::set_command_identity` with an owned `ShellBytes` name and
`CommandIdentity::Builtin` to report that name as a shell builtin from
`command -v`, `command -V`, and `type`. This does not change dispatch or
placement. Names with `/`, such as `/bin/cp`, are looked up as paths without
using identity metadata or `PATH`.

When an external handler is installed, epsh passes expanded arguments and the
complete child environment as byte pairs. Redirections have already been
applied to file descriptors. The environment includes inherited entries unless
env_clear was selected, exported shell variables, and command-prefix
assignments.

`ShellBuilder::environment` and `Variables::from_environment` use the same import
rules as the default inherited environment. Non-shell variable names remain
available to children. An explicit empty environment imports no inherited values;
`IFS` retains its normal default. Shells never mutate the host environment.
`Variables` can be cloned for validation before replacing a store. Its `binding`
accessor exposes value and flags; `unexport` retains a local value, and `declare`
creates an unset local binding without replacing an existing one.

## Arithmetic

Arithmetic uses signed 64-bit values. Addition, subtraction, multiplication,
and negation wrap. Division or remainder by zero, division or remainder of the
minimum value by -1, and shift counts outside 0 through 63 return an arithmetic
error. Left shift keeps the low 64 bits; right shift is arithmetic. Compound
arithmetic assignments use the same rules. A failed operation leaves its
assignment destination unchanged, while effects from the right-hand side stay.

For interactive integrations, eval_command can return
ShellError::Stopped { pid, pgid }. run_program converts a stopped command to
exit status 148; see [process handling](docs/process-model.md).

If an embedder constructs a Pipeline with no commands, eval_command returns
ShellError::Runtime with the pipeline's source span. run_program reports the
error and returns status 2.

## Command-line runner

Run a command string, script file, or input from standard input:

    epsh -c 'echo hello'
    epsh script.sh
    cat script.sh | epsh
    epsh -s < script.sh

Short options can be combined: -e exits on an eligible failure, -u errors on
unset variables, -x traces commands, -f disables pathname expansion, and -n
parses without executing. The -o and +o forms enable and disable pipefail,
errexit, nounset, xtrace, noglob, and noexec. Running epsh without arguments
from a terminal prints usage. The runner has no -h or --help option.

## Scope and help

epsh does not provide prompts, line editing, history, aliases, job-control
builtins, here-strings, extended globs, arrays, typeset, or select. Traps run
between commands, not while the shell is blocked waiting for a child. The
~user form of tilde expansion remains literal; a leading ~ expands from HOME.

For help, see the [project repository](https://github.com/joshuarli/ish) or
report an issue in its [issue tracker](https://github.com/joshuarli/ish/issues).
epsh is distributed under the [MIT license](LICENSE).
