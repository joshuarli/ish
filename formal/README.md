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
Policy cases include static `cp` and `mv` routes and run registered
parent-process targets through epsh in parent, pipeline, and command-substitution
contexts. The runtime probe checks status, redirection creation, and target
policy and external-handler callbacks. Run the policy comparison and placement
probe with `cargo run --example semantic_bridge -p ish -- --family policy --cases 1000`.
The coverage matrix records each adapter's domain. A detected semantic
mismatch is reduced and saved under
`formal/fixtures/bridge-regressions/`.

`make verify` also compiles four isolated production mutations and requires the
bridge to detect each one. The [mutation guide](../docs/verification/mutation-testing.md)
describes the detectors and their Python requirement. Run `make verify-refinement`
on Linux to extract and prove the production shift-count helper. Its first run
downloads a separate development toolchain under `.work/`.

See the [coverage matrix](../docs/verification/coverage.md) for bridge domains
and correspondence limits, [expansion generation](../docs/verification/expansion-generation.md)
for generated inputs, [mutation testing](../docs/verification/mutation-testing.md)
for production controls, and [shift refinement](../docs/verification/refinement.md)
for the extracted proof and its trust boundary.
