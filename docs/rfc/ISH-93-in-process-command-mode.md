# ISH-93: In-process command mode

Status: accepted

## Summary

`ish -c COMMAND` will run `COMMAND` with the embedded epsh library inside the
ish process. It will not invoke `/bin/sh` or spawn an `epsh` binary. The
login-shell CLI contract stays: stdin, stdout, stderr, and the exit status
behave as they do today. The interactive prompt keeps its smaller syntax and
its dispatch gate.

## Motivation

The current entry point execs `/bin/sh -c COMMAND` in
[`src/main.rs`](../../src/main.rs). The [README](../../README.md) and
[ISH-63](ISH-63-login-shell-command.md) record that choice. A user pasted a
`for` loop at the prompt and got `ish: for commands are not supported` from
`dispatch::plan()`. The same loop runs through `/bin/sh`, but ish cannot run it
with the engine it already ships.

epsh evaluates a parsed program in the calling process. `Shell::run_script` and
`Shell::run_program` are in
[`crates/epsh/src/eval.rs`](../../crates/epsh/src/eval.rs). With no sink,
builtin output goes straight to fd 1 and fd 2. `Variables::new()` inherits the
process environment. The parser already accepts loops, functions, and
redirections. No subprocess is needed.

## Design

1. Keep `parse()` in `src/main.rs` unchanged. `-c` keeps its one operand and
   its existing errors.
2. Convert the operand with
   `ShellBytes::from_os_str(command).to_shell_string()`, matching the epsh CLI.
   Parse with `epsh::parser::Parser`. On parse failure, report `ish: {error}`
   and exit 2.
3. Build `epsh::eval::Shell` with `Shell::new()`. Set `$0` to `ish` with
   `set_args_bytes`. Run the parsed `Program` with `run_program`. Exit with
   `status.code()`.
4. Do not install output sinks. epsh writes builtin output to the inherited
   descriptors. External commands inherit stdin, stdout, and stderr through
   the default executor.
5. Leave `dispatch::plan()` alone. The interactive gate still refuses control
   flow and functions. Command mode uses epsh directly and accepts full epsh
   syntax.
6. Preserve the login-shell contract: no config load for `-c`, no OSC 7 or
   OSC 133 output, no extra positional operands, and a propagated exit status.
7. Keep library runtime diagnostics as they are. `run_program` prints
   `epsh: ...` for runtime errors. Parse errors use `ish: ...`.
8. Update the docs with the code: README top and CLI section, architecture,
   ISH-63 status, the AGENTS invariant, and the terminal-interop matrix.
9. Add tests in `tests/login_shell.rs`: loops and functions produce expected
   output, `$0` is `ish`, and a parse error exits 2 with `ish:`. Keep the
   `printf`, `exit 37`, `cat`, and argument-error checks passing. Keep the
   no-OSC PTY check in `tests/pty.rs`.

## Alternatives considered

- Keep `/bin/sh` delegation. Rejected. The owner wants command mode to run on
  the shell ish ships.
- Spawn the `epsh` binary with `-c`. Rejected. It still uses a subprocess and
  needs a second installed executable.
- Send command strings through `dispatch::plan()`. Rejected. The gate refuses
  loops and functions, so the requested script would fail again.
- Add a second ish mode that parses scripts. Rejected. It duplicates the
  embedding path without a user-visible gain.

## Open questions

1. Set `$0` to `ish` or leave the library default `epsh`? Recommend `ish`. It
   gives a stable signal that no `/bin/sh` handled the string.
2. Accept arguments after `COMMAND` as `$1` and beyond? Recommend no. Current
   ish rejects them, and the login-shell callers here do not need them.
3. Prefix runtime diagnostics with `ish:`? Recommend keeping `epsh:`. Changing
   it needs a sink in epsh, which stops direct descriptor writes.
4. How to prove the absence of `/bin/sh` in CI? Recommend a `$0` assertion and
   a source check. `/bin/sh` is an absolute path, so clearing `PATH` does not
   test it.
5. How far does epsh parity need to go for `ssh`, `scp`, `cron`, and `make`?
   Recommend the existing login-shell tests plus one multiline script. epsh
   is a POSIX subset, not a byte-for-byte `/bin/sh`.

## Decision

The owner chose in-process epsh for `-c`. This replaces the interpreter choice
in ISH-63 while keeping its login-shell CLI contract. The open questions are
implementation defaults. Resolve them with the recommendations above unless
the owner says otherwise.
