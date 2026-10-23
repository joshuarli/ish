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
implementation into `fdu-posix`. Linux engine and fdu callers use it. ARM64
musl acceptance subsequently passed in the Linux VM run below. Native macOS
acceptance belongs to ISH-45.

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
warning. Foreign-owner, deferred-flag, and Btrfs clone acceptance ran in Linux
containers with the required capabilities and are recorded below. The ext4
FICLONE probe returned `EOPNOTSUPP`; the loop-backed Btrfs case passed. No
native ARM64 or macOS result is claimed. SIGKILL recovery does not establish
power-loss durability.

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
| FICLONE and GNU comparison | Passed on a loop image formatted as Btrfs using the host x86_64 Linux kernel. The tests, method selection, metadata comparison, and sync trace are detailed in [Btrfs clone and GNU `cp` comparison](#btrfs-clone-and-gnu-cp-comparison). |

### Btrfs clone and GNU `cp` comparison

The source revision was `5ef8940d858e6f85e298dd98e88f745948a20caf`. The host
was Linux x86_64 with kernel `7.2.9-2-stable`. The test runner used the Alpine
3.24.1 `ish-builder:gate-repairs` image. A privileged container loaded the
host's Btrfs module, formatted a 1 GiB loop image with Btrfs 6.17.1, and
mounted it at the archived checkout's `.work/tmp`. The image file was under
`.work/tmp/acceptance-clone` on host ext4. The container had root,
`CAP_SYS_MODULE`, and `CAP_SYS_ADMIN`. It unloaded the module and detached the
loop device after the run.

The named FICLONE case passed:

```sh
cargo test -p ish-fs --lib public_copy_selects_reflink_when_ficlone_works -- --ignored --nocapture
```

The case asserted two cloned files and `786432` logical bytes. The metadata
parity case also passed on Btrfs:

```sh
cargo test -p ish-fs --lib public_into_copy_methods_preserve_identical_bytes_metadata_and_hardlinks -- --nocapture
```

It checks bytes, uid/gid, mode, mtime, ACLs, xattrs, symlink target bytes, and
hardlink identity across automatic, native, and buffered copy methods. On
Btrfs, it also requires the automatic method to select Clone.

GNU Coreutils 9.11 `cp --archive --reflink=always` copied the same fixture:

```sh
strace -f -e trace=ioctl,fsync,fdatasync,syncfs,sync_file_range \
  cp --archive --reflink=always \
  "$fixture/left/alpha" "$fixture/right/beta" \
  "$fixture/readonly" "$fixture/raw-link" "$fixture/out-gnu/"
```

`strace` recorded two successful `FICLONE` ioctls. The GNU output matched the
source and public-engine outputs for bytes, ownership, mode, mtime, ACL, xattr,
symlink target bytes, and hardlink identity. The trace had no `fsync`,
`fdatasync`, `syncfs`, or `sync_file_range` calls. A public-engine copy with
`Options::durable = true` also selected Clone and issued `fsync` calls. ish
documents the durable guarantee under `--durable`; the GNU command above made
no sync request.

The source patch was not pushed. Run `39792421234` tested the unchanged `dfb6154`
base before these repairs: x86_64 and ARM64 musl failed the runtime-compiled
byte fixtures because direct `rustc` could not find `cc`; semantic verification
failed because its mutation matcher no longer matched the production source;
macOS failed to compile against unavailable rustix APIs. The local x86_64 musl
and semantic gates above pass on the completion branch, but no hosted run has
tested that branch.

## ARM64 Linux musl acceptance — 2026-10-23

The unchanged runtime at `d9c33ef` passed on ARM64 through OrbStack's Linux VM
on an Apple silicon macOS host. The guest kernel was
`7.2.9-orbstack-00391-g2b298d05fbd4`, running aarch64 instructions without
cross-architecture emulation. This is VM evidence, not a bare-metal Linux run.
The Alpine 3.24.1 builder used the pinned `nightly-2026-09-15` musl toolchain,
kache 1.0.0, and mold 2.39.1. No runtime changes were needed for this run.
Cargo used `RUSTC_WRAPPER=kache`,
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=clang`, and
`RUSTFLAGS='-L native=/usr/lib -Clink-arg=-B/usr/lib/e-crt/aarch64-unknown-linux-musl -Clink-arg=-B/usr/local/libexec/mold'`.

The workspace check and strict all-target Clippy passed. The initial tests on
the macOS shared-directory projection failed raw-name, ACL, ownership, and
sparse-allocation assertions. Those results do not establish Linux behavior.
The successful rerun mounted an owned loop-backed ext4 image over `.work/tmp`
inside the container. All backing files and mountpoints stayed under `.work/`.
The ext4 and Btrfs mounts and the script's temporary mounts were unmounted
after their checks.

| Acceptance | Command and observed result |
| --- | --- |
| Workspace check | `cargo check --workspace` passed. |
| Strict Clippy | `cargo clippy --workspace --all-targets -- -D warnings` passed. |
| Workspace tests | `cargo test --workspace --lib --tests --no-fail-fast` passed with ext4 fixtures and an absolute `TMPDIR`. |
| Integrated shell tests | `make test TARGET=aarch64-unknown-linux-musl CARGO_CMD=cargo` passed with the same musl linker flags. |
| Owned mount fixtures | `benches/fs/native-fixtures.sh` passed all five cases: EXDEV move, replacement aliases, nested mounts, bind aliases, and bind-mount recovery refusal. |
| Foreign-owner EXDEV | With uid 0 and an owned tmpfs destination, `cargo test -p ish-fs --test native_boundaries native_boundary_foreign -- --ignored --nocapture` passed both cases, including the privilege-dropped refusal. |
| Deferred inode flags | With `CAP_LINUX_IMMUTABLE` on ext4, `cargo test -p ish-fs --lib repair_deferred_metadata_validates_successful_output -- --ignored --nocapture` passed. |
| Btrfs clone | On an owned loop-backed Btrfs image, `cargo test -p ish-fs --lib public_copy_selects_reflink_when_ficlone_works -- --ignored --nocapture` passed. The copy-method bytes, metadata, and hardlink parity test also passed on Btrfs. |

These checks close ISH-32's remaining ARM64 musl acceptance. They establish
process-crash recovery on the tested filesystems, not power-loss durability.
macOS acceptance remains separately tracked under ISH-45.

## Native macOS ARM64 acceptance (2026-10-23)

The native run used macOS 27.0.1, Darwin 27.0.0, ARM64, and APFS on
`/System/Volumes/Data`. Tests ran as uid 502 with an absolute `.work/tmp`
`TMPDIR`, the pinned Rust nightly, and kache. Noninteractive sudo required a
password, so privileged ownership acceptance did not run.

The repairs use supported entropy, PTY, pipe, FIFO, and ACL APIs. Preflight
now shares the engine's version comparison, which excludes access time while
retaining every other observed field. Resource-fork streams close before the
final xattr size query. The buffered fallback pre-sizes its output before
sparse writes. ACL clearing uses an empty ACL through `acl_set_fd_np`;
`acl_delete_fd_np` returned ENOTSUP and left the old ACL installed on this host.

The private clone tests previously assumed `O_EVTONLY` bypassed read
permissions. Native probes disproved that assumption. Automatic copying now
declines cloning before creation for owner-unreadable sources or source/parent
ACLs. A public-copy regression preserves bytes, mode 000, and the source ACL
through the ordinary path. This trades clone eligibility for safe access;
it does not use a pathname permission mutation after an identity check.

| Acceptance | Command and observed result |
| --- | --- |
| Workspace check | `cargo check --workspace` passed. |
| Strict Clippy | `cargo clippy --workspace --all-targets -- -D warnings` passed. |
| Workspace tests | `cargo test --workspace --lib --tests --no-fail-fast` passed after the startup repairs and accepted macOS umask boundary described below. Required privileged and fixture-only exclusions remain explicit. |
| Interactive PTY tests | `cargo test -p ish --test pty` passed all 178 cases, including copy cancellation and foreground terminal restoration. Staging polling drains PTY output so progress cannot backpressure the test's shell. |
| Filesystem targets | Native ish-fs library, policy, and preservation targets passed. Transfer passed all 12 cases. Native macOS passed 24 cases, with the foreign-owned staging case explicitly ignored pending root access. Restrictive masks now assert the accepted rejection behavior. |
| Large resource fork | A complete 4,294,971,392-byte named-fork copy passed with a 1 MiB buffer. Assertions checked the length, bytes above `u32::MAX`, and zero content in an interior hole. This does not assert physical resource-fork sparsity. |
| Required identity failure | A live Unix socket descriptor passed `fstat` but its filesystem query returned EINVAL. The native identity boundary propagated the error instead of substituting a device-only identity. |
| Raw path names | APFS returned EILSEQ while creating malformed UTF-8 names. Those fixture legs report exclusion only for that error; valid control-name, symlink, hardlink, and canonical trust-path assertions still execute. Linux raw-name acceptance is recorded above. |
| Integrated shell tests | Ordinary-user `make test` passed after ISH-61's evaluator startup repair. The retained native conformance corpus remains required, with its existing host exclusions. |
| Release build | The earlier `make release TARGET=aarch64-apple-darwin` run passed with build-std and the actual release profile and flags. That arm64 Mach-O binary was 1,549,696 bytes; the later startup repairs were not rebuilt in release mode. |
| Panic diagnostic | A scratch probe built with the same release profile, target, and build-std flags aborted with status 134 and printed `ISH45_RELEASE_PANIC_PROBE` and `src/main.rs:2:5`. Only the probe shell disabled core dumps. No build-time or runtime performance claim is made. |

Integration reuses these native results. Focused ARM64 Linux checks cover the
shared version predicate, environment/history tests, prompt fixture, and
foreground-job fixture. The final foreground fixture uses rustix's typed
controlling-terminal operation because Darwin and musl declare different ioctl
request integer types.

### Startup and staging follow-up

ISH-61 reproduced as a concurrent parent/child `setpgid` failure: Darwin returned
EPERM in the child even when the requested group had already been assigned.
Checking the resulting group did not eliminate every failure. The child now
owns group assignment and reports startup before the parent starts another
evaluator. A 4,096-subshell regression failed before the repair and passes
afterward. Startup acknowledgement waits also honor cancellation and deadlines;
a pending-writer timeout regression failed before that correction and passes now.

Parallel evaluator tests exposed an EOF launch gate kept open by unrelated
fork-inherited descriptors. Pipeline launch now uses socket shutdown to release
readers. The complete epsh library and integration suite passes. The PTY
completion assertion now waits for grid and preview states before checking its
unchanged selection requirements; all 178 PTY tests pass.

The owner chose to keep filesystem operations in-process and reject macOS masks
that remove owner read or search permission (`0500`). The earlier macOS
restrictive-umask success expectations changed to require an `Unsupported`
error, unchanged source and existing entries, and no destination publication.
Masks `0400`, `0100`, and `0777` are rejected; `0200` remains supported. On this
macOS 27 host, the public read-only `getumask` preflight rejects before creating
quarantine state. Actual-mode checks also reject inaccessible directories
without changing their identity or permissions. Older hosts or concurrent mask
changes can leave an inaccessible session identified by its diagnostic; see
[staging permissions](filesystem.md#macos-staging-permissions).

Focused Linux ARM64 musl evaluator, conformance, foreground-job, and lifecycle
tests passed. The Linux restrictive-umask recovery and transfer cases passed on
the native ext4 fixture. The transfer case returned EACCES on the macOS-shared
mount, so that shared-filesystem result is not native Linux acceptance. Linux
retains its existing restrictive-umask success contract.

### Apple copy comparison

The fixture was a 64 MiB APFS file with two 4 KiB sentinels, mode 0440, an
explicit staff-read ACL, a custom extended attribute, and a 22-byte resource
fork. Native hole punching created the sparse fixture before copying.

| Side | Observed result |
| --- | --- |
| Public ish-fs engine | Complete with durability enabled; 0 Clone, 1 Native, and 0 Buffered files. Bytes, mode, mtime, ACL, EA, resource fork, and sparse allocation matched the source. `bytes_streamed` was 67,108,864 and logical cloned bytes were zero; streamed bytes do not measure physical I/O. |
| `/bin/cp -p` | Exit 0; exact bytes, mode, mtime, ACL, EA, resource fork, and sparse allocation matched. |
| `/bin/cp -p -c` | Exit 0; exact bytes, mode, mtime, EA, resource fork, and sparse allocation matched, but the source ACL was absent. |

The source and each output reported 72 blocks of 512 bytes. The engine declined
cloning because the source had an ACL. Apple's `-c` requests cloning and can
fall back; the utility's actual selected syscall was not traced because native
tracing required root. Neither utility command requested or established the
engine's durability guarantee. These are preservation observations, not a
speed comparison or power-loss test.

### Remaining macOS acceptance

Only the native privileged ownership branches remain unrun. Noninteractive sudo
requires a password on this host. Build the test executable as the ordinary user,
then run the two foreign-owner clone branches and the explicit staging case as
root from the repository root:

```sh
ish_native_test=$(
    TMPDIR="$PWD/.work/tmp" CARGO_TARGET_DIR="$PWD/.work/macos-target" \
        cargo test -p ish-fs --test native_macos --no-run --message-format=json |
        jaq -r 'select(.reason == "compiler-artifact" and .target.name == "native_macos") | .executable // empty'
)
sudo "$ish_native_test" clone_declines --nocapture
sudo "$ish_native_test" --exact private_directory_prepares_foreign_owned_staging_directory --ignored --nocapture
```

The clone cases exercise foreign ownership by dropping credentials in isolated
children. An ordinary-user run cannot establish those root-only branches.

## Remaining native evidence

| Deferred run | Setup and acceptance condition |
| --- | --- |
| Native macOS | [ISH-45](../TODO.md) retains only privileged ownership acceptance. The accepted restrictive-umask boundary, complete ordinary-user workspace/integrated gates, native cases, and utility comparison are recorded above. |

The repair run passed native x86_64 musl workspace checks, strict Clippy, Rust
and PTY tests, conformance, semantic verification, and shift refinement.
ARM64 musl and ARM64 macOS passed full workspace strict all-target cross-Clippy.
Selected ARM64 ish-fs tests ran under QEMU. Self-spawning umask/descriptor-limit
fixtures ran directly as isolated emulated children because binfmt was absent.
Those earlier cross-checks did not satisfy native acceptance; the later ARM64
Linux VM run is recorded above.

Real full/quota-limited destinations, filesystems without sparse support, and
power-loss durability remain unproven. Injected storage errors, allocation
feedback, and SIGKILL tests do not establish those guarantees. Lean models do
not prove filesystem durability or callback/kernel behavior.
