# Release

The compiler pin is [`rust-toolchain.toml`](../rust-toolchain.toml): nightly with `rust-src`, `llvm-tools`, and `clippy`. `cargo build --release` uses the prebuilt standard library. `make release` rebuilds `std` with the `dist` profile.

## Local builds

On macOS arm64:

```sh
make release TARGET=aarch64-apple-darwin
make verify-release TARGET=aarch64-apple-darwin
make install TARGET=aarch64-apple-darwin
```

`make install` copies `target/$TARGET/dist/fz` to `~/.local/bin/fz` and, on Darwin, ad-hoc signs that copy. On Linux musl, `verify-release` requires a stripped static binary with no ELF interpreter and no `NEEDED` entry. It skips those checks for other targets.

Linux release builds use the image in [`Dockerfile`](../Dockerfile). The [release workflow](../.github/workflows/release.yml) builds that image, then runs `make release` and `make verify-release` inside it.

## Published archives

The workflow is manual and runs only on the default branch. It builds macOS arm64, Linux x86_64 musl, and Linux arm64 musl. It runs `make test-ci` for the macOS build only. It then publishes prerelease `release-<commit-sha>` with `SHA256SUMS` and the two static Linux zips named in the [installation instructions](../README.md). The macOS binary is not attached.
