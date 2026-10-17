# Testing

Run the full Rust test suite with:

    cargo test

For a focused check, choose the test target that covers the change:

    cargo test --test api_stability
    cargo test --test embedding
    cargo test --test integration

Unit tests also live beside the implementation and run under cargo test.
Integration behavior tests are in [integration.rs](../tests/integration.rs);
embedding API checks are in [embedding.rs](../tests/embedding.rs); public API
surface checks are in [api_stability.rs](../tests/api_stability.rs).

The filtered mksh conformance suite uses the built CLI:

    cargo build
    perl check.pl -p ./target/debug/epsh -s check-epsh.t

Regenerate the filtered suite from check.t with:

    perl filter-tests.pl check.t > check-epsh.t

The stress runner compares output and elapsed time against dash:

    sh tests/stress/run.sh ./target/debug/epsh dash

Its timings are exploratory and vary with host load; they are not fixed
performance gates.
