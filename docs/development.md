# Build and check ish

Use the toolchain pinned in [rust-toolchain.toml](../rust-toolchain.toml).
Linux musl builds use mold. Rust compilations use the configured kache wrapper.
Keep scratch files under the ignored `.work/` directory:

```sh
mkdir -p .work/tmp
export TMPDIR="$PWD/.work/tmp"
make build
```

[Makefile](../Makefile) owns build and release commands.
[Cargo.toml](../Cargo.toml) defines workspace members and default members.
`make build` and `make test` select the defaults. To cover every member, run:

```sh
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib --tests --no-fail-fast
```

Use a named crate, test target, or test filter during iteration. See
[PTY tests](ptytest.md), [epsh testing](../crates/epsh/docs/testing.md), and
[filesystem testing](filesystem-testing.md) for focused commands.
`make check` runs workspace Clippy, Rust tests, and `make verify`.
[The formal guide](../formal/README.md) covers semantic checks and refinement.

`make release` builds a static release; `make release-dynamic` builds its dynamic
variant. `TARGET` selects the target triple. Use the corresponding
`verify-release` target to inspect Linux linking. These commands use build-std
and require the pinned toolchain's rust-src component.

`make install` installs the release binary in `~/usr/bin` and signs it on macOS.
`make install-login-shell` also edits `/etc/shells` and changes the account's
login shell, using sudo or doas when needed. Run that target only when you
intend to change account state.

## Accepted release policy awaiting implementation

[ISH-42](rfc/ISH-42-release-delivery.md) records the accepted decisions for
release dispatch, final-archive checksums, verified build-tool downloads, and
panic diagnostics. The existing workflow and build flags still need that work;
the RFC includes its validation and rollback requirements.
