# ISH-26: Trust for sourced denv files

Status: accepted; implemented with Linux acceptance for both evaluators
(2026-10-22).

## Decision

Choose **option A: retire denv's implicit file-loading helpers**. Keep the
existing automatic discovery and content approval of `.envrc` and `.env`, and
keep `.env` loading in Rust's assignment parser. Do not add a dynamic import
broker, a transitive dependency trust database, or another shell interpreter.

Retire all six helpers, including the optional variant omitted by the draft:

- `source_env` and `source_env_if_exists`
- `source_up` and `source_up_if_exists`
- `dotenv` and `dotenv_if_exists`

Replace their bodies with diagnostic-only rejecting stubs in both evaluators.
A stub names the removed helper and explains the migration; it never opens,
sources, or executes a target, searches ancestors, or falls through to an
external command. Optional forms are also errors, even when a target is absent.
These stubs provide migration diagnostics, not compatibility implementations.

The stubs invalidate the **whole candidate environment**, not merely the call.
A successful later command, `|| true`, or an `if` condition must not make that
candidate acceptable. Propagate through epsh-owned forks and bash's own
subshells, pipelines, and command substitutions. Arbitrary newly executed
interpreters are outside this boundary. Use an evaluation-owned failure latch:
a dedicated inherited failure channel for bash and forked evaluation can report
the retired-helper invocation to the owning evaluator alongside its existing
snapshot channels. A variable or final exit status alone is insufficient across
subshells. Use small fixed-size records and nonblocking writes; a full channel
already contains failure evidence. The reporter must never block waiting for
the evaluator to drain it. Keep this an error signal, not a request/response
service, and reuse an internal typed-failure transport where available.
Honor the existing evaluation deadline and reap owned children on failure.
A timeout, malformed channel result, or incomplete result must not apply the
candidate. Do not wait indefinitely for EOF from a descendant holding a writer.

## Supported trust boundary

Trust applies to the exact bytes of the files denv itself discovers and loads.
`read_trusted_candidate` must remain the common read/hash boundary; execute or
parse those approved bytes rather than reopening the path after checking it.
If either discovered file is untrusted, modified, or denied, load neither.
No `.env` contents pass through shell `.` or `eval`; shell metacharacters in
values remain data under the existing assignment grammar.

An approved `.envrc` is arbitrary executable code with the user's authority.
It can explicitly run another interpreter or use that interpreter's `.` builtin;
denv does not sandbox those operations or recursively authorize everything that
code might execute. Document this limit beside the README's trust explanation.
The removed helpers must not imply that a parent or shared file receives denv
approval merely because a child `.envrc` calls it. Deliberately written direct
sourcing is the `.envrc` author's responsibility and carries no per-file denv
revocation guarantee.

Rejecting a candidate prevents its variable assignments from entering the
interactive shell. It cannot undo files, processes, or other external effects
already caused by trusted code. Leaving the previous directory may still restore
the previous denv layer, and the shell may mark the new directory dirty; keep
those transitions separate from applying the rejected candidate. This matches
[ISH-15's explicit-environment decision](ISH-15-environment-and-shell-ownership.md).

## Migration and scope

- For ordinary `.env`, remove the redundant `dotenv` call. Keep assignments in
  the automatically discovered `.env` and approve that file with `denv allow`.
  Preserve the existing order: `.env` assignments override `.envrc` exports.
- For shared setup or parent inheritance, put the intended setup in the one
  discovered `.envrc` and approve its resulting contents. A user who deliberately
  writes direct sourcing accepts responsibility for downstream code; do not
  offer that as an equivalent checked-import mechanism.
- Retain unrelated stdlib helpers such as `PATH_add`. Do not apply ISH-24's
  interactive builtin restrictions to the denv script evaluator.
- Keep `denv allow`/`deny` focused on discovered files. Do not silently approve
  parent files, arbitrary paths, or a dependency closure.

This is an intentional compatibility change. Existing scripts that use these
helpers must be edited and approved again. In particular, replace
`denv_dotenv_helper_loads_env_file` with rejection and automatic `.env` loading
coverage; keeping that permissive test would contradict the decision.

## Implementation and acceptance

1. Remove all helper loader bodies, install the rejecting diagnostics, and
   propagate their failure independently of the script's final status through
   both epsh and bash evaluation, including forked contexts. Failure-channel
   setup and cleanup must not leak descriptors or extend the timeout. Apply
   accepted candidates only through the explicit shell environment.
2. Test every retired name with existing and missing files. Include an
   untrusted parent reached through `source_up`, a previously approved parent
   later denied or edited, and a changed shared file. The target must never be
   opened/executed by the helper, and no candidate variable changes may apply.
3. Test direct calls, `helper || true`, a helper inside a conditional, and a
   command substitution/subshell, in both interpreter paths. Preceding and
   following exports must not leak from a rejected candidate; the shell remains
   usable and shows a diagnostic naming the helper. Flood the failure channel
   with repeated calls and check that the evaluator deadline still works.
   Extend low-fd remapping coverage to avoid collisions with snapshot fds 3/4
   and their duplicated sources; exercise repeated evaluations and descendants
   retaining the new descriptor, with bounded cleanup and no fd leaks.
4. Test automatic trusted `.env` loading without a helper, alongside `.envrc`,
   on its own, and after edit/revocation. Preserve precedence, previous-layer
   restoration, and dirty-state reporting. Values containing `$()`, backticks,
   quotes, and semicolons must not create a marker file or execute a command.
5. The README documents the trust boundary, removed helpers, automatic `.env`
   behavior, and failure effects. The environment transport preserves rejection
   semantics without process-environment mutation.

## Why not checked imports?

Preserving the helpers would require checking every dynamically selected file,
executing exactly the checked bytes in both interpreters, handling recursion,
recording dependencies, and revalidating them on cache hits as well as reloads.
A child file's unchanged mtime or approval cannot establish the trust of that
closure. The added machinery is disproportionate to ish's small environment
feature. Retiring the helpers makes the supported boundary explicit and removes
the current unchecked convenience paths.

Transitive trust by default and hashing a guessed dependency list at
`denv allow` time are rejected. The owner decision is complete; retain this RFC
as the decision record after implementation.
