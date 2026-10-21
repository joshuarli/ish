# Testing

Build, test, or package epsh from the workspace root. Select `-p epsh` because
the crate shares ish's workspace and lockfile:

    cargo build -p epsh
    cargo package -p epsh

Run its library and integration tests:

    mkdir -p .work/tmp
    TMPDIR="$PWD/.work/tmp" cargo test -p epsh --lib --tests

Tests that use fixture executables compile them with `rustc` into the test's
temporary directory. Set `RUSTC_WRAPPER` to wrap these compilations. The same
test command works from an unpacked Cargo package archive.

Run the native conformance corpus and print each case result:

    TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance -- --nocapture

Run one case by ID:

    EPSH_CONFORMANCE_ID=glob-range-3 TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance legacy_conformance -- --exact --nocapture

Run cases with one corpus category:

    EPSH_CONFORMANCE_CATEGORY=shell:legacy-no TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance legacy_conformance -- --exact --nocapture

The native runner classifies all 175 retained IDs without Perl and executes
host-applicable cases. Exclusions are reported separately and do not count as
passing executions. It keeps case data as bytes and creates isolated working
directories under the repository's `.work/tmp`, independently of ambient
`TMPDIR`. Standalone packages use their own `.work/tmp`. The runner reports
host-category exclusions. A `need-pass: no` case still runs; a mismatch is
reported as an ignored failure. `info-pre` prints informational output and does
not skip a case. The corpus and its license notice are in
[check-epsh.t](../check-epsh.t) and [tests/conformance/NOTICE](../tests/conformance/NOTICE).

The workspace targets run the same checks:

    make test-epsh
    make conformance

Focused Rust targets include [integration.rs](../tests/integration.rs),
[embedding.rs](../tests/embedding.rs), and [api_stability.rs](../tests/api_stability.rs).
Command-policy coverage is in the `command_policy` module of `embedding.rs`.

The stress runner compares output and elapsed time against dash:

    TMPDIR="$PWD/.work/tmp" sh crates/epsh/tests/stress/run.sh "$PWD/target/debug/epsh" dash

Its timings are exploratory and vary with host load. They are not fixed
performance gates.

## Lean model and Rust bridge

Run semantic verification from the workspace root:

    make verify

The formal project pins Lean in [lean-toolchain](../../../formal/lean-toolchain).
Its theorems establish properties of the defined model, not the Rust
implementation. The differential driver compares six model families with
actual Rust entry points for 1,000 generated inputs per family by default.
Unsupported inputs and fuel exhaustion remain distinct outcomes. `make verify`
builds the formal models, checks protocol fixtures, audits theorem axioms, and
runs the bridge and isolated production mutations. See [the protocol](../../../formal/PROTOCOL.md)
and [the evaluation model](../../../formal/IshSemantics/Evaluation/README.md)
for the wire contract and model limits.
