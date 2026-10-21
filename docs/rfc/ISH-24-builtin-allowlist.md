# ISH-24: Builtin allowlist

Status: draft

## Summary

Replace ish's deny-list of epsh builtins with an allowlist, so that only
builtins the README documents can run inside the shell process.

## Motivation

`dispatch::plan()` and the dynamic target check refuse `eval`, `source`, `.`
and ish shell-state builtins. Every other name in epsh's `BUILTIN_NAMES` runs in
ish's own process. `exec` replaces ish with the target program and ends the
session. `trap` replaces ish's SIGTERM, SIGHUP and SIGWINCH handlers with
epsh's, whose pending-trap flag ish never reads. `set` options, `wait`, `read`,
`umask`, `getopts` and `local` also run without an ish-level decision. A
contract that exposes only the README syntax cannot rest on a list of names to
refuse, because each new epsh builtin is exposed by default.

## Design

Define the allowed epsh builtins in one place in ish. `plan()`, the dynamic
check and completion read it. A name outside the list is refused with status 2
before evaluation, as `eval` is today. The refusal covers literal and
dynamically selected command names.

The first list contains the builtins the README names: `set`, `export`,
`unset`, `echo`, `pwd`, `true` and `false`. Everything else is refused until a
design says otherwise. The README says "including", so the list must be
reconciled with what users rely on before it is enforced.

## Alternatives considered

- Add `exec` and `trap` to the deny-list. This fixes the two proven cases and
  leaves the next epsh builtin exposed by default.
- Add a policy hook in epsh that disables trap installation and process
  replacement. This helps other embedders but does not decide which builtins ish
  exposes.

## Open questions

1. Which of `read`, `umask`, `wait`, `getopts`, `local` and `set` options
   belong in the first list? Recommend none beyond the README's list, after
   checking which others users run.
2. Should the allowlist live in `builtin.rs` beside the ish registry? Recommend
   yes, so one table answers "what runs in the shell".

## Decision

Pending. Implementation must add `dispatch::plan` tests for `exec`, `trap` and
a dynamically selected `exec`, update the README, and delete this RFC.
