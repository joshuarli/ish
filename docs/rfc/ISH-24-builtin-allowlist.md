# ISH-24: Builtin allowlist

Status: accepted; implemented with focused Linux acceptance (2026-10-22).
Native macOS was not exercised.

## Decision

Use an explicit ish builtin policy in `src/builtin.rs`, shared by planning,
expanded dispatch, completion, and command lookup. Keep epsh's unrestricted
standalone API and denv evaluators separate from this interactive policy.
New epsh builtins are unavailable in ish until deliberately added to the policy.

The allowed epsh commands are:

| Commands | Accepted purpose |
|---|---|
| `echo`, `printf`, `pwd`, `true`, `false`, `:` | Output, current directory, and status |
| `test`, `[` | Conditions usable with the already-supported `&&`, `||`, and `!` |
| `kill` | Explicit process signaling; no ownership of job waits or signal handlers |
| `export`, `unset` | Variables in epsh's store |
| `set` | List exported values, or set and export a named variable as specified below |
| `command` | Existing execution and `-v`/`-V` lookup forms, subject to the same policy |

The existing ish registry remains authoritative for ish-owned names and their
execution classes. In particular, `cd` retains its hybrid whole-line/list form,
and `exit` and `type` use ish's whole-line handlers. An ish entry does not grant
permission to call an identically named epsh builtin outside that entry's
allowed placement. Keep the existing `cp`/`mv` parent-process restrictions.

Reject every other epsh builtin, including `exec`, `trap`, `eval`, `source`,
`.`, `read`, `umask`, `wait`, `getopts`, `local`, `readonly`, `shift`, `return`,
`break`, and `continue`. Refusal is status 2 with a diagnostic. Do not fall
through to PATH for a refused builtin name. An explicit executable path remains
an ordinary external command; this policy is an interactive language boundary,
not a sandbox for user programs.

## Restrict `set` arguments

Preserve the existing useful interactive forms:

- `set` lists exported variables.
- `set NAME [value...]` sets and exports `NAME`; no values means an empty value,
  and multiple values retain the current space-joining behavior.
- `NAME` must match `[A-Za-z_][A-Za-z0-9_]*`. Values may start with `-` or `+`.

Refuse every option or positional-parameter form: `set -e`, `set +e`,
`set -o pipefail`, `set -o noexec`, `set -`, `set --`, and a non-name first
operand. Do not permit an option prefix followed by an otherwise valid name.
This deliberately replaces the existing `set_option_forms_fall_through_to_epsh`
PTY expectation. The config-file `set NAME value` directive is unchanged.

These controls prevent changes to evaluator execution modes, while preserving
interactive variable management. There is no new shell option interface in this
work.

## Enforcement and lookup

1. `dispatch::plan()` rejects statically known violations, including nested
   literal `command` execution, before starting the input line. Preserve its
   traversal of pipeline stages, lists, and command substitutions.
2. After word expansion, validate the target **and arguments** before target
   redirections, assignments applied for dispatch, or target execution. Repeat
   through each nested `command` execution layer, before opening even the
   outer command's redirections. The current name-only `CommandPolicy` cannot
   validate `set`; add a minimal opt-in argv policy hook to epsh, retaining the
   existing name-only API for other embedders. Preserve raw argument bytes and
   use the same argument parsing as `command` dispatch.
3. Apply the policy to all dispatch paths, including pipeline children,
   substitutions, aliases, and dynamically constructed names or `set` operands.
   The hook follows [ISH-8's callback boundary](ISH-8-callback-process-boundary.md).
   Prefer owned policy data evaluated inside epsh. Any registered child callback
   still needs ISH-8's explicit audit: immutable captures alone do not establish
   fork safety. Never bypass a refusal with an unrestricted handler.
4. Completion, `is_builtin`, `w`/`which`/`type`, and `command -v`/`-V` consult
   the same exposure policy. Lookup may describe a supported whole-line command
   without executing it. Refused epsh names are not advertised as runnable
   builtins or as PATH fallbacks that a bare invocation would then reject.
   Explicit paths still resolve normally.

A runtime refusal cannot undo earlier commands or command substitutions used
while expanding arguments. State that limit in the README; do not claim that an
entire input line is transactional. A surrounding `!` may invert status 2 under
existing expression semantics; the diagnostic and lack of target effects remain.

## Implementation and acceptance

- Define the allowed names, execution placement, and `set` argument rule once;
  replace the current deny-list and the completion union of all epsh names.
- Test literal, quoted, aliased, and variable-selected `exec` and `trap`; test
  nested `command command ...`, pipelines, and substitutions. A rejected target
  must not create/truncate its redirection file or replace the shell. Confirm
  ish's SIGTERM handling still works after a refused `trap`.
- Test allowed `set`, empty values, option-looking values, and raw value bytes;
  reject static and expanded option/positional forms without changing options,
  positionals, or target redirection files. Replace the permissive PTY test with
  these observable assertions.
- Test every approved command class, `command -v`/`-V` and completion agreement,
  a refused builtin with a same-named PATH executable, and explicit-path escape.
  Preserve native `cp`/`mv` and hybrid `cd`/`history` behavior.
- In particular, `command -v exit` may describe ish's whole-line command while
  `command exit` is refused. `command -v exec` and `command -V trap` report
  unavailable names rather than epsh builtins or PATH fallbacks. Cover `--`,
  `command -- exec`, and `command -p command -- exec`; use execution and query
  rules separately, rather than applying an execution callback to query names.
- Update the README to enumerate the finite set and restricted `set` forms when
  implemented. Keep epsh's standalone and denv behavior covered separately.

## Rationale and rejected alternatives

A deny-list exposes every future epsh addition automatically. The draft's
seven-name list would also remove documented `command` support and existing
`printf` use. The selected set keeps useful interactive output, conditions,
lookup, and signaling while refusing scripting state and process replacement.
Name-only filtering is insufficient because `set` includes both environment
operations and persistent evaluator options.

The owner decision is complete. ISH-24 stays open until implementation and its
acceptance checks land; retain this RFC as the decision record.
