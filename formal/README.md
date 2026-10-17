# Lean models and Rust bridge

The Lean project models selected behavior from ish and epsh. Most theorems state
properties of those models. A separate [shift-count refinement](../docs/verification/refinement.md)
checks freshly extracted production Rust against the arithmetic model.
See the [formal and Rust coverage matrix](../docs/verification/coverage.md)
for the theorem links, production entry points, regression commands, and bridge
domains.

## Check the models

The project pins Lean 4.35.0. Run `lake check --paranoid` from `formal/` to
recheck the default library with all bundled kernels. The library imports every
model family and its proof fixtures; the axiom and coverage audit remains a
separate check. Kernel checking requires Linux namespaces and bubblewrap.

Run `lake build semanticBridge protocolTests` from `formal/` to build the models
and executables. Run `lake env lean IshSemantics/Audit.lean` from `formal/` to
check theorem axioms and verify that the coverage matrix links to audited
declarations. Run `lake exe protocolTests` from `formal/` to check the protocol
fixtures and byte round trips.

## Compare Rust entry points

Run the differential bridge from the repository root:

```sh
cargo run --example semantic_bridge -p ish -- --family all --cases 1000
```

The command generates 1,000 semantic requests for each of the policy,
expansion, arithmetic, evaluation, effects, and interactive families. It also
runs retained regression cases, protocol boundary checks, and one comparator
control per family. A family comparison fails if either side returns
`unsupported_domain` or `fuel_exhausted`. Separate effects boundary checks
require those outcomes for invalid environment inputs and a zero-fuel request.
The coverage matrix records each adapter's domain. A detected semantic
mismatch is reduced and saved under
`formal/fixtures/bridge-regressions/`.

The bridge calls production Rust entry points for each family. Its generated
domains are bounded. Expansion supplies controlled filesystem entries,
evaluation supplies command observations, and effects checks selected raw-byte,
environment, cwd, and redirection operations subject to host OS preconditions.
The expansion adapter requires command substitutions to leave parent variables
and cwd unchanged. Its model retains the substitution status but discards child
binding deltas; arithmetic effects can update parent bindings. Returned errors
abort expansion, while a nonzero command status remains a successful substitution
result. The bridge compares projected
`arithmetic` and `expansion` error categories; it does not claim exact Rust error
variant correspondence. Typed cancellation propagation has a direct Rust test.
The evaluation adapter checks that a bare assignment returns status 0 when no
command substitution runs and the final substitution status otherwise, using
bounded `false` and `true` assignment cases.
The [coverage matrix](../docs/verification/coverage.md) describes those limits
for each adapter.

Expansion retains its first 53 fixtures and then uses a seeded schedule covering
315 combinations of separator bytes, delimiter boundaries, and quoting contexts.
The [generation reference](../docs/verification/expansion-generation.md) lists the
dimensions and limitations.

`make verify` also compiles four isolated production mutations and requires the
bridge to detect each one. The [mutation guide](../docs/verification/mutation-testing.md)
describes the detectors and their Python requirement. Run `make verify-refinement`
on Linux to extract and prove the production shift-count helper. Its first run
downloads a separate development toolchain under `.work/`.

The mismatch minimizer accepts only validated candidates with a strictly
smaller serialized request key. It tracks visited candidates to avoid shrink
cycles. Focused runner tests cover semantic versus nonsemantic outcomes,
boundary rejection, strict decrease, and repeated shrink candidates.

The Rust PUA codec and the Lean octet map are separate representations. The
bridge tests their selected observable behavior. The byte codec has no Rust
refinement proof. The separate extracted proof covers shift-count validation
and conversion only.
