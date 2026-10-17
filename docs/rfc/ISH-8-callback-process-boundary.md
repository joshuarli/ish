# ISH-8: Callback process boundary

Status: draft

## Summary

Define where embedding callbacks execute when epsh forks an evaluator, and
prevent callbacks from silently inheriting unusable host synchronization state.

## Motivation

An external handler can capture an ordinary mutex and satisfy its current Send
bound. Command substitution forks, retains that handler, and invokes it in the
child. If another host thread held the mutex at fork, releasing it in the
parent cannot unlock the child's copy. The demonstrated callback blocks until
the shell deadline even though its owner released the lock much earlier.

The existing stdout guard addresses a known library lock. It cannot establish
that arbitrary callback captures are safe after fork. Command-policy and
redirect-open callbacks also need an explicit process boundary; the reproduced
failure concerns the external handler in command substitution.

## Design

Treat callback process placement as part of the public embedding contract.
Inventory each callback invocation in pipelines, subshells, background execution,
and command substitution before choosing an enforcement mechanism.

The default must not silently run a general host callback in a forked evaluator
when its captured state has no established child-safe contract. One bounded
option is to reject that invocation with a typed diagnostic unless the embedder
explicitly supplies a callback under a child-execution safety contract. Define
which operations such a contract permits; Send alone is insufficient.

If normal callbacks must remain available in forked evaluators, execute them
through a parent-owned broker or change the execution boundary. A broker must
preserve raw arguments, child environment, working directory, descriptor effects,
cancellation, and status, without allowing parent and child to wait on each
other indefinitely. That is a larger design than adding a trait bound.

## Alternatives considered

- Fail closed for callbacks reached after fork, with explicit child-safe opt-in.
  This gives a bounded implementation but changes existing embedding behavior.
- Broker callbacks through the parent. This preserves host-owned state but adds
  a protocol, descriptor transfer, and lifecycle responsibilities.
- Document that multi-threaded embedding is unsupported. Documentation alone
  does not enforce the restriction and leaves safe public APIs easy to misuse.

## Open questions

1. Is rejecting unsupported child callback execution acceptable? Recommend the
   bounded fail-closed default with an explicit safety contract. Choose a broker
   only if existing embedding requirements need general callbacks there.
2. Must all callback kinds have identical placement? Recommend one documented
   boundary, with any exceptions justified by the callback's actual effects.

## Decision

Pending. Validate the selected contract with a host lock held across fork and
released before the deadline, plus relevant pipeline and substitution paths.
Move the implemented contract into the process model and public API docs, then
delete this RFC.
