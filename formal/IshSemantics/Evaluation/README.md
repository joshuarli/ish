# Evaluation model reference

`IshSemantics.Evaluation.evalRequest` accepts the shared version-1 request and
returns `Outcome Lean.Json`. Success contains the shared encoded `Execution`.
The evaluator uses an independent `Expr` and explicit leaf observations.
The model does not call Rust or Lean's process APIs.

## Input

The `input` object has exactly three required fields:

| Field | Value |
| --- | --- |
| `program` | An expression described below |
| `observations` | An array of objects with exactly `status`, `stdout`, and `stderr` |
| `options` | An object with exactly Boolean `errexit` and `pipefail` |

Observation status is an integer from 0 through 255.
Output fields use the shared byte-array encoding.
Every expression rejects unknown fields and missing fields.
Assignment names must be unique within each simple expression.
The expression decoder limits nesting to 1024 levels.
The request's explicit `fuel` bounds evaluation depth independently of decoding.
Zero evaluation fuel returns `fuel_exhausted` before evaluating any command.
Fuel exhaustion makes no termination or success claim.

Each expression uses one of these exact shapes:

| `tag` | Other required fields |
| --- | --- |
| `simple` | `assignments`: name-byte pairs, `argv`: byte arrays, `observation`: array index |
| `sequence` | `left`, `right`: expressions |
| `and` | `left`, `right`: expressions |
| `or` | `left`, `right`: expressions |
| `not` | `body`: expression |
| `subshell` | `body`: expression |
| `pipeline` | `commands`: expression array, `bang`: Boolean |

An empty `argv` applies assignments to the current state and preserves the previous
status, matching epsh's literal assignment path. The evaluator ignores its observation index.
A nonempty `argv` selects the indexed observation and emits an invocation event.
Nonempty stdout and stderr emit writes to descriptors 1 and 2, in that order.
Prefix assignments affect the invocation environment and leave shell variables unchanged.
Prefix assignments override exports, and exports override inherited environment entries.
Bare assignments also update an existing export's value.
Environment entry order is representational and must be canonicalized for Rust comparisons.

## Production AST mapping

The differential driver must parse `source` with epsh's actual parser.
The driver must derive the model expression from that AST and run the same parsed
program through `Shell::run_program`. The driver must reject a supplied expression
that disagrees with the parsed program. Directly interpreting this JSON in Rust
would not establish correspondence with the production evaluator.

The admitted source domain contains literal assignments and literal argument bytes,
with no redirections, expansions, builtins, functions, traps, or asynchronous commands.
Literal and quoted word parts concatenate into one argument.
An empty quoted argument remains an argument.
The driver must reject unquoted splitting, globbing, tilde expansion, and substitutions.
The external callback supplies the declared observation and records the actual
invocation environment, working directory, and output effects.
Observation indices identify syntactic leaves. Skipped leaves consume no observations.
Different leaves can share an index when their oracle observations agree.

AST `Simple`, `Sequence`, `And`, `Or`, `Not`, `Pipeline`, and redirection-free
`Subshell` map directly to their expression counterparts.
Program command lists fold into `sequence` in source order.
An empty program needs a separate driver observation or an explicit unsupported outcome.
The mapper must not create a synthetic external invocation for an empty program.
Brace groups can map to their bodies only when the driver explicitly admits them
and verifies that they have no redirections.
All other syntax falls outside this evaluation model.
The shared syntax and policy model checks syntax admission separately.

## Status, state, and traces

Sequential composition appends output and ordered events, returning the right
command's status and state. AND and OR evaluate their left side in a tested context.
A skipped right side adds no effects. Negation tests its body and inverts its status.
Errexit halts at untested failing simple commands, pipelines, and subshells.
A compound command does not independently trigger errexit from its returned status.
Tested context propagates through sequences and suppresses nested leaf checks.
Negated pipelines suppress errexit both inside the pipeline and on its inverted result.

A pipeline with one stage executes that stage in the current state, matching epsh.
A pipeline with multiple stages evaluates every stage from the same parent state.
Only the pipeline's final status returns to the parent state.
Without pipefail, the final stage selects the status.
With pipefail, the rightmost failing stage selects the status, or zero if all succeed.
Subshells similarly discard body state changes and retain the returned status.

Pipeline traces concatenate each stage's local trace in stage order.
Each stage's descriptor 1 denotes its local output, including output directed into a pipe.
The model does not describe descriptor wiring or process scheduling.
The model returns the final stage's stdout and concatenates stage stderr in stage order.
These choices are projections, not assertions about kernel event order.
Compare pipeline status and parent state separately from process traces.
Compare pipeline output only when the fixture makes scheduling and pipe consumption irrelevant.
Ordered sequential traces remain comparable when commands contain no pipeline or subshell fork.

The admitted leaf oracle returns statuses and bytes without callback errors.
Expansion, redirection, process creation, cancellation, and callback failures require
other models or explicit unsupported outcomes. A modeled semantic error retains its
error, state, and trace. `prepend` also retains all earlier trace events on an error.
The error-composition theorem does not establish that every production failure is modeled.

## Checked properties

`Evaluation.lean` proves general skipping, composition, status selection, tested-context,
assignment-scope, and state-isolation properties. `Tests.lean` checks concrete boundaries.
`Audit.lean` prints the dependencies of every named theorem.
The audit permits only Lean's standard axioms: `propext`, `Quot.sound`, and `Classical.choice`.
No theorem uses `sorry`, `admit`, custom axioms, mathlib, or `native_decide`.

Run the focused checks from `formal`:

```text
lake build IshSemantics.Evaluation.Tests IshSemantics.Evaluation.Audit
```

These checks establish properties of the model. Production correspondence additionally
requires the Rust differential driver and its admitted AST mapping.
