# Filesystem testing

[The file operation reference](filesystem.md) owns user-visible behavior.
Run focused ish-fs tests from the workspace root:

```sh
cargo test -p ish-fs --lib
cargo test -p ish-fs --test policy
cargo test -p ish-fs --test native_linux -- --nocapture
```

## Coverage locations

Use the assertions in these files rather than inferring coverage from test names:

| Behavior | Test source |
| --- | --- |
| Mapping, collisions, and non-mutating dry-run | [policy.rs](../crates/ish-fs/tests/policy.rs) |
| Bytes, raw names, metadata, links, and bounded traversal | [transfer.rs](../crates/ish-fs/tests/transfer.rs), [preservation.rs](../crates/ish-fs/tests/preservation.rs), [resources.rs](../crates/ish-fs/tests/resources.rs) |
| Publication, rename, and source retirement | [publication.rs](../crates/ish-fs/tests/publication.rs), [move_safety.rs](../crates/ish-fs/tests/move_safety.rs) |
| Transfer faults, mutation, cancellation, and cleanup | [engine/tests.rs](../crates/ish-fs/src/engine/tests.rs), [engine/repair_tests.rs](../crates/ish-fs/src/engine/repair_tests.rs) |
| Indexed lookup and single-pass dry-run work | [records.rs](../crates/ish-fs/src/engine/records.rs), [anchored.rs](../crates/ish-fs/src/engine/anchored.rs), [dry_run.rs](../crates/ish-fs/src/engine/dry_run.rs) |
| Platform copying and preservation | [native_linux.rs](../crates/ish-fs/tests/native_linux.rs), [native_macos.rs](../crates/ish-fs/tests/native_macos.rs) |
| Real EXDEV, ownership, and mount boundaries | [native_boundaries.rs](../crates/ish-fs/tests/native_boundaries.rs) |
| SIGKILL publication | [interruption.rs](../crates/ish-fs/tests/interruption.rs) |
| Negation, placement, discovery, and terminal lifecycle | [foreground_placement.rs](../crates/epsh/tests/foreground_placement.rs), [pty.rs](../tests/pty.rs), [PTY guidance](ptytest.md) |

Receipts must retain the successfully transferred destination version through
publication, deferred metadata, hardlink changes, and merge aggregation.
The mutation regressions check content, mode, access metadata, and aliases.
Later mismatches retain remaining sources and completed destinations.
Hardlink regressions include symlink objects, cross-operand groups, skips,
merges, and mixed rename/EXDEV ordering. Gated concurrency tests cover unrelated
transfers, representative failure, and cancellation without stranded waiters.

Operation-count tests cover indexed lookup, directory reopening, and dry-run
enumeration. Resource tests exercise deep and wide trees under bounded limits.
Those assertions establish structural scaling; timing evidence belongs to the
[benchmark report](../benches/fs/results-linux.md).

## Run owned Linux mount fixtures

```sh
benches/fs/native-fixtures.sh
```

The script requires unprivileged user/mount namespaces, `unshare`, `mount`, and
`umount`. It creates disposable tmpfs and same-device bind mounts below `.work/`
in a private namespace, then unmounts before cleanup. It never mounts over user
data. For manual fixtures, follow ownership and path checks in
[native_boundaries.rs](../crates/ish-fs/tests/native_boundaries.rs).

Every `native_boundaries` case that needs a fixture is `#[ignore]`, so
`cargo test -p ish-fs --test native_boundaries` and `make test` report those
cases as ignored rather than passed. The script runs the EXDEV move, EXDEV
replacement-alias, nested mount, and bind-alias cases with `-- --ignored` and
the `ISH_FS_TEST_*` variables set. An ignored case run without its variables, or on a fixture that
does not satisfy it, panics. The `native_macos` case that needs uid 0 is ignored
the same way; the other macOS cases fail instead of returning early on a
filesystem without APFS clones or resource forks.

Two ish-fs lib cases need capabilities the script cannot provide without sudo,
so they are `#[ignore]` and panic when the capability is missing: the reflink
case and the immutable-flag case run from the rows below. The sparse and
`copy_file_range` lib cases run unconditionally and panic on a filesystem
without those capabilities. The xattr and Linux ACL fixtures in the copy-method
parity and repair lib cases likewise panic when the filesystem rejects them.

The foreign-owner cases need uid 0 with CAP_CHOWN. The script's single-ID user
namespace cannot assign uid 65534, so it does not run them; see the
foreign-owner row below.

## Completed Linux x86_64 evidence

The local acceptance run used the `dfb615457ad4828216d9d8b8ea44711e8806b633`
base plus the working-tree patch identified in the completion report. The host
was Linux x86_64, kernel 7.2.9-2-stable, Alpine musl, on ext4. The workspace
filesystem was device 66307. The candidate was not pushed, so no hosted CI run
tested this patch.

| Acceptance | Command and observed result |
| --- | --- |
| Workspace check | `cargo check --workspace` passed. |
| Strict Clippy | `cargo clippy --workspace --all-targets -- -D warnings` passed. |
| Workspace tests | `cargo test --workspace` passed after the replacement-alias, merge-progress, blocking-flag, and shell-rendering regressions were added. |
| Integrated shell tests | `make test TARGET=x86_64-unknown-linux-musl` passed in `ish-builder:gate-repairs`; runtime-compiled epsh byte fixtures passed with the image's configured clang linker. |
| Conformance | `make conformance` passed: 168 passed, 7 declared feature exclusions, 0 failed, out of 175 retained cases. |
| Semantic verification | `make verify` passed: Lean checks and bridge passed, and all four compiled production mutations were behaviorally killed, including dynamic command checking after redirection setup. |
| Shift refinement | `make verify-refinement` passed with the pinned Lean 4.35.0 toolchain. |
| Owned mount fixtures | `benches/fs/native-fixtures.sh` passed real ext4-to-tmpfs move and replacement-alias tests, nested mount refusal and explicit traversal, mountpoint EBUSY retirement, and bind-mount hardlink topology cases. The script's private namespace supplied its disposable mount fixtures. |
| Foreign-owner EXDEV | In `ish-builder:gate-repairs`, a private tmpfs cross root was mounted with `CAP_SYS_ADMIN`; the runner had uid 0 and `CAP_CHOWN`, `CAP_SETUID`, and `CAP_SETGID`. `cargo test -p ish-fs --test native_boundaries native_boundary_foreign -- --ignored --nocapture` passed both the readable foreign-owner move and privilege-dropped refusal tests. The refusal preserved source identity and bytes and published no destination. |
| Deferred inode flags | With `CAP_LINUX_IMMUTABLE` on host ext4, `cargo test -p ish-fs --lib repair_deferred_metadata_validates_successful_output -- --ignored --nocapture` passed immutable and append-only hardlink-group, merge, EXDEV move, injected failure, and cancellation cases. |
| FICLONE | The required named ignored test was executed with `cargo test -p ish-fs --lib public_copy_selects_reflink_when_ficlone_works -- --ignored --nocapture`. It failed because host ext4 returned `EOPNOTSUPP` (error 95). No clone-capable fixture was available, so successful clone acceptance remains pending. |

The final source patch is local and uncommitted. The completed GitHub run
[39792421234](https://github.com/joshuarli/ish/actions/runs/39792421234) tested
the unchanged base revision before these repairs and failed all four jobs. The
local x86_64 musl and semantic gates above pass on the candidate; that does not
establish hosted ARM64 or macOS results.

## Required native evidence still missing

| Required run | Setup and acceptance condition |
| --- | --- |
| Successful Linux reflink | Use an owned FICLONE-capable filesystem, then run the named test above and confirm the public engine selects `Clone` while preserving required logical metadata. The available ext4 host returned `EOPNOTSUPP`; the test did not pass. |
| Native ARM64 musl | On ARM64 Linux musl hardware, run workspace check, strict all-target Clippy, workspace library/integration tests, `make test`, and `benches/fs/native-fixtures.sh`. Cross-build and QEMU execution are supporting checks, not native hardware evidence. |
| Native macOS | macOS acceptance is tracked in [TODO.md, ISH-45](../TODO.md). Run workspace check, strict all-target Clippy, workspace tests, `make test`, and the native suite as an ordinary user; run privileged cases separately with suitable ownership and capabilities. Require clone/access normalization, ACLs, foreign ownership, flags, cancellation, sparse fallback, and resource forks to execute. Validate large resource forks above 4 GiB and unsupported identity failure. |
| Native comparisons | On clone-capable Linux and macOS, follow the [benchmark harness](../benches/fs/README.md). Record selected methods and preservation/durability differences for GNU `cp` and Apple `cp`/`cp -c`. The available host provides BusyBox `cp`, not GNU clone-aware `cp`, and no Apple utility. |

The repair run passed native x86_64 musl workspace checks, strict Clippy, Rust
and PTY tests, conformance, semantic verification, and shift refinement.
ARM64 musl and ARM64 macOS passed full workspace strict all-target cross-Clippy.
Selected ARM64 ish-fs tests ran under QEMU. Self-spawning umask/descriptor-limit
fixtures ran directly as isolated emulated children because binfmt was absent.
These results do not satisfy the native runs above.

Real full/quota-limited destinations, filesystems without sparse support, and
power-loss durability remain unproven. Injected storage errors, allocation
feedback, and SIGKILL tests do not establish those guarantees. Lean models do
not prove filesystem durability or callback/kernel behavior.
