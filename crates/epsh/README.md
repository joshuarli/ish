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

Shell values preserve arbitrary bytes; operations at OS and C-string
boundaries may reject NUL. Use set_var_bytes and get_var_bytes when Rust code
needs raw variable values; get_var returns a string only when the value is
valid UTF-8. See [byte preservation](docs/byte-preservation.md).

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
| external_handler(...) | Delegate external command execution to the embedder. |
| redirect_open_handler(...) | Let the embedder open writable redirection targets. |
| interactive(bool) | Enable foreground terminal handoff and stopped-process detection for pipelines. |

When an external handler is installed, epsh passes expanded arguments and the
complete child environment as byte pairs. Redirections have already been
applied to file descriptors. The environment includes inherited entries unless
env_clear was selected, exported shell variables, and command-prefix
assignments.

For interactive integrations, eval_command can return
ShellError::Stopped { pid, pgid }. run_program converts a stopped command to
exit status 148; see [process handling](docs/process-model.md).

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

For help, see the [project repository](https://github.com/joshuarli/epsh) or
report an issue in its [issue tracker](https://github.com/joshuarli/epsh/issues).
epsh is distributed under the [MIT license](LICENSE).
