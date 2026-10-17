# Lean models and Rust bridge

The Lean project models selected behavior from ish and epsh. Its theorems state
properties of those models. They do not establish that the models cover the
Rust implementations. See the [formal and Rust coverage matrix](../docs/verification/coverage.md)
for the theorem links, production entry points, regression commands, and bridge
domains.

## Check the models

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

The command generates 1,000 cases for each of the policy, expansion,
arithmetic, evaluation, effects, and interactive families. It also runs
retained regression cases, protocol boundary checks, and one negative control
per family. A detected semantic mismatch is reduced and saved under
`formal/fixtures/bridge-regressions/`.

The bridge calls production Rust entry points for each family. Its generated
domains are bounded. Expansion supplies controlled filesystem entries,
evaluation supplies command observations, and effects checks selected raw-byte,
environment, cwd, and redirection operations subject to host OS preconditions.
The [coverage matrix](../docs/verification/coverage.md) describes those limits
for each adapter.

The Rust PUA codec and the Lean octet map are separate representations. The
bridge tests their selected observable behavior; it does not derive a Rust
refinement proof from the Lean theorem. Aeneas and Charon are not available in
this toolchain, so no Rust-to-Lean translation or refinement proof is claimed.
