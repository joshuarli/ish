# ISH-63: Login-shell command mode

Status: accepted; implemented.

## Summary

ish must satisfy the POSIX login-shell contract that every non-interactive
SSH, `scp`, `cron`, and `make` invocation relies on: `SHELL -c COMMAND` runs
`COMMAND`. ish delegates that string to `/bin/sh -c` and never parses or
evaluates it. `--config PATH` selects the config file; `-c` is the command.

## Motivation

`make install-login-shell` registers ish in `/etc/shells` and sets it as the
account's login shell. sshd then runs `ish -c '<remote command>'` for a
non-interactive connection. ish interpreted `-c` as a config path, tried to
load the remote command as a file, and failed with
`cannot take the terminal: Not a tty` and `No such file or directory`.

The result is that a Linux account whose login shell is ish cannot be used
non-interactively at all. That includes the transport this shell is being used
with, but it is not specific to it.

## Design

- `ish -c COMMAND` replaces the process with `/bin/sh -c COMMAND` and exits
  with that shell's status. Signals, pipes, redirections, and job control
  belong to the child.
- ish does not tokenize, expand, or refuse anything inside `COMMAND`. Its own
  syntax rules, builtins, and refusals do not apply, because ish is not the
  interpreter. The invariant "no script-file or command-string mode" is kept
  in the sense that ish owns neither: it hands the string to the system shell.
- `--config PATH` selects a config file. `-c PATH` no longer means config; the
  two meanings cannot share a flag because sshd's command is the first
  argument after `-c`.
- With no `-c`, ish remains interactive and refuses other arguments as before.

## Alternatives considered

- Treat `-c` as config when its value names an existing file, otherwise as a
  command. A remote command that happens to name a readable path would load it
  as configuration instead of running it. Rejected as ambiguous.
- Require `ForceCommand` or a `Match` block in sshd, or a wrapper script. That
  moves a shell defect into host configuration and still breaks `scp` and
  `cron`, which invoke the login shell directly.
- Implement scripting in ish. That is the design the project deliberately
  rejected, and it is unnecessary to satisfy the contract.

## Open questions

- Should `-c ''` be accepted silently, as `/bin/sh` does, or reported? Today
  it runs an empty shell and exits 0.
- Should ish fall back to `$SHELL` when `/bin/sh` is absent? Every supported
  platform has `/bin/sh`, so a missing one is reported as status 127.
