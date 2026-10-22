# ISH-8: Callback process boundary

Status: accepted; implemented with Linux callback-boundary acceptance
(2026-10-22); native macOS acceptance remains under ISH-45.

## Decision

Callbacks registered through safe APIs execute only in the process that
registered them. Refuse an attempted invocation in an inherited forked
evaluator with a typed diagnostic and status 2. Provide a separate, explicitly
unsafe registration path for a callback whose complete child-execution
contract has been audited. Do not add a parent broker or silently fall back to
another executor, policy or redirection opener.

Apply one boundary to external handlers, command policies (including the
argv-aware policy required by [ISH-24](ISH-24-builtin-allowlist.md)) and
redirect-open handlers. This decision remains the contract. The callback
boundary is implemented and described in epsh's process model and public API
documentation.

## Why

`Send` permits moving a callback between threads; it does not make captured
host state usable after fork. The current external callback can inherit a
mutex held by a vanished thread during command substitution. Unlocking it in
the parent cannot release the child's copy. `fork_evaluator` coordinates
known stdout and descriptor-reader locks, not arbitrary callback state.

A broker would require argument/environment serialization, descriptor
transfer, cancellation and a new parent/child progress protocol. That cost is
not justified for the present embedding API. Explicit refusal gives ordinary
host callbacks a deterministic boundary without promising transparent access
to host synchronization from child evaluators.

## Enforcement

- Record the registering process identity with each callback. Check the real
  process identity at each invocation; a shell flag alone misses an embedder
  that forks outside epsh. Cover pipelines, subshells, background evaluation,
  command substitution, nested `command` dispatch and externally forked
  copies. Calling a safe callback from another thread in its original process
  remains allowed subject to the existing Rust borrowing/thread bounds.
- Preflight all callbacks needed for a selected command before opening its
  target redirections or dispatching it. For redirection-only commands,
  preflight the complete redirection set before opening any target. A refused
  policy must not be skipped; a refused redirect provider must not fall back
  to an ordinary truncating open. The diagnostic names the callback kind and
  child-process restriction.
- Propagate the typed refusal to the evaluating shell and map it to status 2
  at the public numeric boundary, including failures reached through command
  substitution. It must not turn into an empty successful substitution or
  permit the enclosing target's effects. Preserve the documented ordering:
  prior commands and already completed expansion effects are not rolled back.
  Carry the refusal on a private close-on-exec control channel; an ordinary
  command exiting 2 still has normal substitution semantics. Do not infer
  this error from stdout, stderr or a reserved numeric exit status.
- Check immediately before invocation as well as during dispatch preflight.
  Registration replacement preserves the same rule. Do not invoke or drop an
  inherited disallowed callback merely to remove it: its captured values may
  have arbitrary destructors. Child exit/cleanup must avoid running those
  destructors; parent-owned callback cleanup remains in the parent.
- Keep `CommandPlacement::ParentProcess` as a separate command restriction.
  Child-capable callbacks cannot override that restriction, including its
  logical pipeline/substitution checks when an evaluator stays in process.

## Explicit child execution and ish integration

The unsafe registration API must state the actual obligation: every operation
reachable from the callback, including destructors and error paths, must be
valid in the child at every possible fork point. No inherited host lock,
logging/runtime service, environment mutation, or dependency on a vanished
thread is permitted. A function pointer, immutable capture or `Send + Sync`
bound alone does not establish this. Capture changes in the child are local
to that child; the caller cannot expect parent-visible mutations.

Rust's [post-fork safety guidance](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.pre_exec)
also covers allocator, environment and library internals. An unsafe opt-in
must not be justified solely by "this closure has no Mutex". This ticket
eliminates the hidden callback boundary; it does not establish that the
remaining Rust evaluator is universally safe after a multithreaded fork.
Remove any unqualified embedding claim that implies that stronger guarantee.

ish must retain its supported pipelines and substitutions. Prefer owned data
for its command/argument policy, and use the default epsh executor for ordinary
external programs as specified by [ISH-5](ISH-5-foreground-job-outcomes.md).
Audit the remaining ish-owned child callbacks individually, including the
transitive work of native commands and writable redirections. Use unsafe
registration only with an explicit, defensible safety argument; if one cannot
be supplied, move that operation out of the callback path or keep that command
parent-only with a documented refusal. Do not mark the entire current
external-handler closure child-safe by assertion. `cp` and `mv` retain their
existing parent-process restriction. ISH-15's explicit environments remove
one global-state hazard but do not discharge this fork contract.

## Acceptance

- Install each ordinary callback with a host mutex held across fork and
  released in the parent shortly afterward. Child invocation must report the
  typed refusal promptly, never enter the callback, and not depend on timeout
  recovery. Run the probe under an independent watchdog so failure is bounded.
- Cover every fork-producing evaluator path, an embedder-created fork,
  nested `command`, and a redirection-only command. A denied child policy or
  opener leaves target files and external-command markers untouched.
- A refused callback inside substitution reports failure to the parent and
  suppresses the enclosing target; document and test the status/error mapping.
- Ordinary callbacks still work in their registering process. A deliberately
  minimal audited child callback runs in the child and preserves raw argv,
  explicit environment, cwd, descriptors and status; child mutations do not
  appear in the parent.
- Existing ish pipeline/substitution and dynamic-command-policy coverage
  stays green. Test inherited callback destructors with an observable marker
  and verify no child cleanup invokes them. Scope broader evaluator fork
  hazards separately rather than presenting these tests as a general proof.
