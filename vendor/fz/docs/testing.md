# Testing

Run the Rust suites and the Python PTY acceptance harness with:

```sh
cargo test
```

Python 3 is required for the acceptance harness. The Rust tests cover scoring, candidate parsing, ranking, worker behavior, and CLI output. The acceptance harness covers terminal interaction. Its differential cases run when `FZY_ORACLE`, `target/upstream/fzy`, `~/d/fzy/fzy`, or `~/d/fzy/build/fzy` is an executable. Otherwise they skip.

To run the differential cases, build the pinned upstream fzy revision in a disposable copy. The repository's `~/d/fzy` checkout remains read-only:

```sh
mkdir -p .work/tmp
git clone https://github.com/jhawthorn/fzy.git .work/tmp/fzy
git -C .work/tmp/fzy checkout --detach 34b88869d022e861da4846c4463aea3ddfb3ff30
make -C .work/tmp/fzy fzy test
FZY_ORACLE="$PWD/.work/tmp/fzy/fzy" cargo test
```

The Python harness is `tests/acceptance.py`; `tests/acceptance.rs` runs it during `cargo test`. `tests/cli.rs`, `tests/candidates.rs`, and `tests/core.rs` cover the CLI, candidate storage, and core matcher and ranking behavior.
