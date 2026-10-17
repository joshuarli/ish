# epsh

epsh is a non-interactive Unix shell library and command-line runner written in Rust.
Build with cargo build; test with cargo test; build and check mksh compatibility with:

    cargo build && perl check.pl -p ./target/debug/epsh -s check-epsh.t

- Shell working directories are per instance; cd must not change the embedder's process working directory.
- Preserve shell data bytes at OS boundaries; keep shell syntax and variable names as text.
- Treat the embedding API and its tests as the supported contract, even when the parser accepts more syntax.
- Keep names distinctive and shell terminology consistent; name tests after source areas or observable behavior.

Topics:

- Setup, library API, CLI, output, and license: [README](README.md)
- Runtime architecture and shell terminology: [docs/architecture.md](docs/architecture.md)
- Byte-preserving shell values and OS boundaries: [docs/byte-preservation.md](docs/byte-preservation.md)
- Forking, process spawning, cancellation, and interactive integration: [docs/process-model.md](docs/process-model.md)
- Test suites and conformance commands: [docs/testing.md](docs/testing.md)
