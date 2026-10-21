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

The recorded x86_64 Alpine musl run on Linux 7.2.9 exercised ext4-to-tmpfs EXDEV,
nested mount refusal and explicit traversal, mountpoint EBUSY retirement, and
eight mixed rename/transfer hardlink combinations. Foreign-owner setup skipped
because its single-ID namespace cannot assign uid 65534. A passing process
with capability skips does not establish those guarantees.

## Required native evidence still missing

| Required run | Setup and acceptance condition |
| --- | --- |
| Successful Linux reflink | Use a FICLONE-capable owned filesystem. Run the native Linux target above and `cargo test -p ish-fs --lib public_into_copy_methods_preserve_identical_bytes_metadata_and_hardlinks -- --nocapture`. Require actual Clone selection and preservation assertions, without a clone skip. The recorded ext4 host returns EOPNOTSUPP. |
| Foreign-owner success and safe refusal | As effective uid 0 with CAP_CHOWN, set `ISH_FS_TEST_CROSS_ROOT` to an owned writable distinct-device directory and run `cargo test -p ish-fs --test native_boundaries native_boundary_foreign -- --nocapture`. Both the success and privilege-dropped denial cases must execute. The denial case checks unchanged source bytes/identity and no publication. |
| Deferred immutable metadata | Run `cargo test -p ish-fs --lib repair_deferred_metadata_validates_successful_output -- --nocapture` where disposable immutable flags are permitted. EPERM skips on the recorded host. |
| Native ARM64 musl | On ARM64 Linux musl, run workspace check, strict all-target Clippy, workspace library/integration tests, `make test`, and the mount fixture script. Cross-build and QEMU execution are not native hardware evidence. |
| Native macOS | Run workspace check, strict all-target Clippy, workspace tests, `make test`, and `cargo test -p ish-fs --test native_macos -- --nocapture`. Require clone, ACL/access, foreign-owner, immutable-flag, cancellation, sparse, and resource-fork cases to execute with appropriate privileges. Large resource forks above 4 GiB particularly need native identity-gate validation; unsupported identity must fail safely. |
| Native comparisons | Follow the [benchmark harness](../benches/fs/README.md) on clone-capable Linux and macOS. Record selected methods and preservation/durability differences for GNU cp and Apple cp/cp -c. |

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
