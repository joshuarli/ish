# Testing

Run the Rust tests from the workspace root:

    mkdir -p .work/tmp
    TMPDIR="$PWD/.work/tmp" cargo test -p epsh

Run the native conformance suite and print each case result:

    TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance -- --nocapture

Run one case by ID:

    EPSH_CONFORMANCE_ID=glob-range-3 TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance legacy_conformance -- --exact --nocapture

Run cases with one corpus category:

    EPSH_CONFORMANCE_CATEGORY=shell:legacy-no TMPDIR="$PWD/.work/tmp" cargo test -p epsh --test conformance legacy_conformance -- --exact --nocapture

The conformance runner keeps case data as bytes, creates an isolated working
directory under `TMPDIR`, and reports host-category exclusions. The corpus and
its license notice are in `check-epsh.t` and [tests/conformance/NOTICE](../tests/conformance/NOTICE).

Focused Rust targets include [integration.rs](../tests/integration.rs),
[embedding.rs](../tests/embedding.rs), and [api_stability.rs](../tests/api_stability.rs).
Unit tests run with `cargo test`.

The stress runner compares output and elapsed time against dash:

    TMPDIR="$PWD/.work/tmp" sh tests/stress/run.sh ./target/debug/epsh dash

Its timings are exploratory and vary with host load. They are not fixed
performance gates.
