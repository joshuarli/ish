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

## Linux x86_64 implementation and evidence

### Shared quarantine implementation

The completion branch extracts the shared identity and quarantine
implementation into `fdu-posix`. Linux engine and fdu callers use it. ISH-32
remains open because successful clone acceptance and native ARM64 evidence are
still missing, and its native macOS acceptance belongs to ISH-45.

`cargo test -p fdu-posix -p fdu-delete --lib --test recovery --test quarantine`
passed the shared journal tests (two unit tests, seven recovery tests, two
ignored fixtures), all sixteen legacy deletion recovery cases, the deletion
outcome unit test. The final focused engine command was
`cargo test -p ish-fs --lib --test policy --test transfer --test resources --test move_safety --test publication`:
the library passed 99 cases (three ignored), policy 14, transfer 11, resources
four (three ignored subprocess fixtures), move safety six, and publication
three. Focused all-target Clippy passed with warnings denied.
An earlier full extraction sweep also passed interruption, native Linux,
preservation, and unprivileged native-boundary targets. That earlier sweep
encountered a resource sampling race: its metadata-memory child exited before
the parent collected any `/proc` sample; the focused resource rerun passed.

The owned mount fixture script passed all five executed cases, including the
new real bind-mount recovery refusal. SIGKILL tests cover restoration before
unlink and retained records after unlink; they establish process-crash
behavior, not power-loss durability. The fork tests cover independently opened
root locks, an inherited pre-exec descriptor, and rejection of unlocking a
parent's session from a child process. Replacement tests kill the process after
intent publication, exchange, capture, and unlink; competing recovery confirms
that the source-root lock remains held. Nested source/destination session tests
block recursive copy and post-publication retirement. A merge child rename
whose sync fails reports partial source retirement. The final owned mount run
also verifies descendant retirement through separate tmpfs and bind-mount
sessions while the mounted roots remain.
Ordinary directory transfer admitted within the sixteen-descriptor budget used
sixteen sampled engine descriptors after traversal recovery checks were added.
The `.` and `..` source cases retain the original parent descriptor and charge
that extra descriptor before mutation.

The shell trash tests passed all three cases, including a removed entry whose
later directory sync fails: the summary counts the removal and reports a
warning. Foreign-owner and deferred-flag acceptance ran in a Linux container
with the required capabilities and is recorded below. The named FICLONE case
ran on ext4 and received `EOPNOTSUPP`; successful clone acceptance remains
pending. No native ARM64 or macOS result is claimed. SIGKILL recovery does not
establish power-loss durability.

The Linux x86_64 acceptance worktree started from
`943fe4687d48534ff2004e52c32ba3d220ead2e2`. Shell runtime changes are in
`95c15f50cdb9b3847c264fd927f8facbcff88602`, and shared quarantine plus
filesystem engine changes are in
`0494a1e8c457ee69a2281a9f2fbb02e85036d4e6`. The host was Linux x86_64, kernel
7.2.9-2-stable, Alpine musl, on ext4; the workspace filesystem was device
66307. No commit from this completion pass was pushed, so hosted CI has not
tested this branch. The latest hosted run
[`39792421234`](https://github.com/joshuarli/ish/actions/runs/39792421234)
tested `dfb6154` and still failed the direct-rustc linker fixtures and semantic
mutation matcher; its macOS job still failed on unavailable rustix APIs.

| Acceptance | Command and observed result |
| --- | --- |
| Workspace check | `cargo check --workspace` passed. |
| Strict Clippy | `cargo clippy --workspace --all-targets -- -D warnings` passed. |
| Workspace tests | `cargo test --workspace --lib --tests --no-fail-fast` passed after the replacement-alias, merge-progress, blocking-flag, and shell-rendering regressions were added. |
| Integrated shell tests | `make test TARGET=x86_64-unknown-linux-musl` passed in `ish-builder:gate-repairs`; runtime-compiled epsh byte fixtures passed with the image's configured clang linker. |
| Conformance | `make conformance` passed: 168 passed, 7 declared feature exclusions, 0 failed, out of 175 retained cases. |
| Semantic verification | `make verify` passed: Lean checks and bridge passed, and all four compiled production mutations were behaviorally killed, including dynamic command checking after redirection setup. |
| Shift refinement | `make verify-refinement` passed with the pinned Lean 4.35.0 toolchain. |
| Owned mount fixtures | `benches/fs/native-fixtures.sh` passed real ext4-to-tmpfs move and replacement-alias tests, nested mount refusal and explicit traversal, mountpoint EBUSY retirement, and bind-mount hardlink topology cases. The script's private namespace supplied its disposable mount fixtures. |
| Foreign-owner EXDEV | In `ish-builder:gate-repairs`, `ISH_FS_TEST_CROSS_ROOT` pointed to an owned tmpfs on a distinct device mounted with `CAP_SYS_ADMIN`; the runner had uid 0 and `CAP_CHOWN`, `CAP_SETUID`, and `CAP_SETGID`. `cargo test -p ish-fs --test native_boundaries native_boundary_foreign -- --ignored --nocapture` passed both the readable foreign-owner move and privilege-dropped refusal tests. The refusal preserved source identity and bytes and published no destination. |
| Deferred inode flags | With `CAP_LINUX_IMMUTABLE` on host ext4, `cargo test -p ish-fs --lib repair_deferred_metadata_validates_successful_output -- --ignored --nocapture` passed immutable and append-only hardlink-group, merge, EXDEV move, injected failure, and cancellation cases. |
| FICLONE | `cargo test -p ish-fs --lib public_copy_selects_reflink_when_ficlone_works -- --ignored --nocapture` executed on ext4 and failed because `FICLONE` returned `EOPNOTSUPP` (error 95). No clone-capable fixture was available, so successful clone acceptance remains pending. |

The source patch was not pushed. Run `39792421234` tested the unchanged `dfb6154`
base before these repairs: x86_64 and ARM64 musl failed the runtime-compiled
byte fixtures because direct `rustc` could not find `cc`; semantic verification
failed because its mutation matcher no longer matched the production source;
macOS failed to compile against unavailable rustix APIs. The local x86_64 musl
and semantic gates above pass on the completion branch, but no hosted run has
tested that branch.

## Required native evidence still missing

| Required run | Setup and acceptance condition |
| --- | --- |
| Successful Linux reflink | Use an owned FICLONE-capable filesystem, then run the named test above and confirm the public engine selects `Clone` while preserving required logical metadata. The available ext4 host returned `EOPNOTSUPP`; the test did not pass. |
| Native ARM64 musl | On ARM64 Linux musl hardware, run workspace check, strict all-target Clippy, workspace library/integration tests, `make test`, and `benches/fs/native-fixtures.sh`. Cross-build and QEMU execution are supporting checks, not native hardware evidence. |
| Native macOS | Deferred to [TODO.md, ISH-45](../TODO.md), per the Linux-host scope. No macOS build or native acceptance is claimed. The pending run requires ordinary-user workspace checks and tests, separate privileged cases, clone/access normalization, ACLs, foreign ownership, blocking flags, cancellation, sparse fallback, resource forks above 4 GiB, unsupported identity failure, and a release panic probe. |
| Clone-aware utility comparisons | A clone-capable filesystem was unavailable, and the host provides BusyBox `cp`, not GNU clone-aware `cp`. Apple `cp`/`cp -c` requires macOS and is deferred under ISH-45. The comparison remains pending; record selected methods and preservation/durability differences when the required hosts and filesystems are available. |

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
