# epsh

epsh is a non-interactive Unix shell library and command-line runner written in Rust.
The crate remains independently packageable inside ish's Cargo workspace. Run
`cargo test -p epsh --lib --tests` from the workspace root for its Rust tests.
The retained native conformance corpus has 175 byte-preserving cases and no Perl
runner.

- Shell working directories are per instance; cd must not change the embedder's process working directory.
- Preserve shell data bytes at OS boundaries; keep shell syntax and variable names as text.
- Treat the embedding API and its tests as the supported contract, even when the parser accepts more syntax.
- Keep the opt-in command policy disabled by default for existing embedders. Check expanded targets before builtin and function dispatch.
- Preserve the documented i64 arithmetic contract, including wrapping operations and explicit arithmetic errors.
- Keep all 175 retained conformance case IDs and classify expected failures as executed ignored failures, not skipped cases.
- Keep names distinctive and shell terminology consistent; name tests after source areas or observable behavior.

Topics:

- Setup, library API, CLI, output, and license: [README](README.md)
- Runtime architecture and shell terminology: [docs/architecture.md](docs/architecture.md)
- Byte-preserving shell values and OS boundaries: [docs/byte-preservation.md](docs/byte-preservation.md)
- Forking, process spawning, cancellation, and interactive integration: [docs/process-model.md](docs/process-model.md)
- Test suites and conformance commands: [docs/testing.md](docs/testing.md)
