# Lean models and Rust bridge

The Lean project models selected behavior from ish and epsh. Its theorems state
properties of those models. They do not establish that the models cover the
Rust implementations. See the [formal and Rust coverage matrix](../docs/verification/coverage.md)
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
runs retained regression cases, protocol boundary checks, and one negative
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

The mismatch minimizer accepts only validated candidates with a strictly
smaller serialized request key. It tracks visited candidates to avoid shrink
cycles. Focused runner tests cover semantic versus nonsemantic outcomes,
boundary rejection, strict decrease, and repeated shrink candidates.

The Rust PUA codec and the Lean octet map are separate representations. The
bridge tests their selected observable behavior; it does not derive a Rust
refinement proof from the Lean theorem. Aeneas is unavailable, so no
translation from Rust to Lean or refinement proof is claimed.
