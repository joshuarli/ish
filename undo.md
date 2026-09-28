# Implement native, cross-platform filesystem undo in ish

Implement this feature to completion in `~/d/ish`, with the necessary narrowly scoped changes in its sibling `~/d/epsh`. The development host is macOS arm64. Use ish’s existing Dockerfile and build machinery for Linux arm64 validation.

The goal is an integrated `ish-undo` subcrate: ordinary protected filesystem commands and shell redirections become recoverable, without eagerly copying large files’ contents. Provide explicit scoped checkpoints for arbitrary external programs. Deliver working code, integration coverage, documentation, and measured results—not just scaffolding or a plan.

## 1. Scope and engineering constraints

Implement the complete **rootless** design: protected native `rm` and `mv`, writable shell redirections, scoped execution, persistent recovery storage, conditional undo/redo, inspection, retention, and diagnostics.

Do not add preload injection, Endpoint Security, privileged helpers, a daemon, FUSE, volume snapshots, or a filesystem watcher pretending to capture old contents. Endpoint Security is a possible future capture backend, not a dependency or unfinished stub in this implementation. Ordinary external commands remain runnable, but their internal filesystem operations are not automatically captured.

Read the current `AGENTS.md`, `README.md`, `Cargo.toml`, `Dockerfile`, `Makefile`, execution/builtin/job/history code, and relevant tests before editing. Inspect epsh’s evaluator, redirections, byte-preserving argument representation, and execution hooks. Follow applicable instructions in both worktrees and preserve unrelated changes.

Use Rust’s standard library and **rustix wherever its APIs suffice**. Fall back to narrow, documented `libc` wrappers for missing platform functionality. Prefer `OwnedFd`/`BorrowedFd`, descriptor-relative operations, and ordinary synchronous code. No async runtime, parallel parser, generic plugin architecture, or new configuration language. Small dependencies are acceptable when genuinely better than maintaining equivalent code; explain additions.

Use `~/d/undo` as a reference for behavior and operation coverage, not a design to mechanically translate. Preserve attribution for any copied code. Additional reference checkouts belong in a disposable development directory, not vendored into ish.

## 2. Crate ownership and shell integration

Add a workspace member such as `crates/ish-undo`, keeping the dependency direction `ish → ish-undo`. The crate owns filesystem operations, saved objects, journals, checkpointing, replay planning, and platform differences. It must not depend on ish’s parser, history, line editor, or terminal renderer.

A reasonable internal split is native operations, checkpointing, storage, journal, replay, and platform code. Adjust file boundaries to the implementation; avoid a proliferation of tiny abstractions.

Pass resolved state paths, policies, execution identity, cancellation, and output/reporting through small interfaces. Reuse ish’s conventions for `~/.local/share/ish`; do not introduce a macOS Application Support directory or an independent interpretation of HOME. Recovery history must be independent of deduplicated command history.

Integrate native operations at actual post-expansion command resolution, not by inspecting the input line’s first word. Preserve aliases, quoting, globbing, AND-OR lists, pipelines, and byte-preserving arguments. Register the new builtins in lookup/help/completion paths. Explicit executable paths such as `/bin/rm` bypass native capture; do not redefine `command` as a special bypass.

## 3. Transaction lifecycle

One accepted top-level input is the default transaction boundary. Establish context before evaluation and command substitution begin; allocate persistent storage lazily. Record actual mutations, including those preceding a failed command or failed later redirection. Skipped branches and read-only inputs must not create recovery noise.

Propagate identity through pipeline/substitution processes. An inherited `Arc`, mutex, or buffered writer is not cross-process coordination. Use process-safe journal coordination and independently acquired post-fork lock handles where required; do not rely on inherited flock ownership providing mutual exclusion. Keep descriptors close-on-exec and never hold a journal lock while waiting for a command, terminal input, or an expensive copy.

Attach transaction identity to the existing foreground-job lifecycle. A stopped job remains active across `fg`; sealing occurs at actual completion, not merely when the prompt returns. Preserve normal exit statuses, signals, terminal handoff, and pipeline behavior. Long native operations must respond to cancellation without leaving the terminal broken.

Capture scope, preservation strength, and execution state are separate facts. Represent native-only capture versus scoped capture; frozen versus linked versions; active, completed, partial, interrupted, conflicted, and unavailable-volume states. A successful exit does not prove complete capture. Rootless capture cannot promise to follow arbitrary detached descendants indefinitely; document that boundary and retain recoverable pre-images when completion is uncertain.

## 4. Protected native operations

Implement useful, documented `rm` and `mv` behavior, not an entire coreutils replacement. Support `rm` options `--`, `-r`/`-R`, `-f`, `-i`, and `-v`; support `mv` options `--`, `-f`, `-i`, `-n`, and `-v`. Handle multiple operands, destination directories, symlinks, and meaningful exit statuses. Unsupported options must fail explicitly rather than silently execute an unprotected external utility. `rm -f` does not waive preservation failures.

For `rm`, preserve affected regular files before unlinking; preserve symlinks themselves; record removed directories and restorable metadata. Walk recursively with bounded resources and appropriate cancellation. Preserve permission checks and partial-failure behavior. Do not turn recursive removal into renaming the whole tree into a trash directory. Respect mount boundaries and reject unsupported entries rather than claiming full recovery.

For `mv`, preserve anything that will be overwritten, then record the successful namespace transition. A pure same-filesystem rename is a namespace operation: do not recursively snapshot an unchanged tree just to move it. Its inverse should verify the expected object and preserve current contents rather than discard edits made after the move.

Implement cross-filesystem moves as staged destination copies followed by recoverable source removal. Do not remove the source before publication succeeds. These moves inherently involve data transfer; distinguish that from additional backup copying. Compound failures must leave an accurate, recoverable partial transaction.

Reject filesystem roots, invalid self/ancestor moves, and operations that would destroy the active undo store or its necessary ancestors. Test those decisions without executing destructive commands against real protected locations.

## 5. Protect redirections at the actual open

Current epsh applies redirections before calling its external handler. Add a small, optional, generic redirection-open integration point in epsh; do not put ish-specific recovery policy there. Existing embedders retain current behavior when no provider is installed.

The provider must own the actual open and return the resulting descriptor. A notification followed by an unrelated path-based `File::create` is insufficient.

For existing regular files, arrange preservation before truncation or writable exposure, operating on the same verified object. Preserve access checks, creation/exclusive-open behavior, append semantics, descriptor duplication, and left-to-right redirection order. Handle newly created files and cleanup/restoration of temporary shell descriptors on every error path.

Cover every writable redirection currently supported by ish, including stdout/stderr combinations, append, and read/write forms. Writing through a symlink protects the resolved target without replacing the symlink. Preserve complete pre-images for append; restoring an old length alone is unsafe after unrelated writes.

Non-regular I/O targets such as `/dev/null` and FIFOs retain normal redirection behavior and are not treated as regular-file versions. Preservation must not read a stream or block merely to snapshot it.

Keep only the post-state information needed for conditional recovery. On cloning filesystems, retain frozen post-images where content comparison is necessary, with expensive comparison deferred to inspection/replay. Repeated writes, delete/recreate at one path, and later native operations in the same input must reconcile correctly.

## 6. Scoped execution for arbitrary programs

Implement:

```sh
undo run --scope <directory> -- <program> [arguments...]
```

Complete a before-checkpoint, run the program normally through existing execution/job-control machinery, then capture the after-state and retain enough information for a conditional patch. This must protect changes made by external scripts and their normally completing children inside the scope without needing to understand their commands.

The scope covers the wrapped program’s execution interval, not earlier argument expansion. Do not claim to have checkpointed side effects of a command substitution evaluated before the checkpoint. Outer native redirections still belong to the surrounding transaction. Execute argv directly; do not invent an `ish -c` scripting mode or reparse a reconstructed command string.

Checkpoint regular files, directory structure/metadata, and symlinks without following directory symlinks outside the scope. Detect unsupported mount crossings, store overlap, unreadable required data, and exhausted budgets before launching the program. Do not silently ignore `.git` or other ordinary workspace contents.

Use frozen clones or bounded independent copies, never a hard-link mirror. Retain before/after catalogs and defer content comparisons where useful; size/mtime equality alone must not cause a changed version to be discarded. Avoid eagerly hashing whole trees merely to finish a clone-backed checkpoint.

Describe this honestly as observed changes within a selected tree and interval—not exclusive process attribution, a sandbox, or an atomic application-consistent snapshot. Concurrent changes and detached writers may make recovery conflict or remain uncertain; neither should erase the checkpoint.

## 7. Preservation and filesystem fidelity

Use explicit preservation kinds, such as frozen clone, frozen copy, and linked inode. Do not hide the backend behind a generic copy call whose actual behavior is unknown.

On macOS, prefer descriptor-based `fclonefileat` for regular files. On Linux, use rustix’s `ioctl_ficlone` where supported. Probe capabilities for actual source/store filesystems and handle real operation errors. A filesystem name alone is not proof of capability.

On filesystems without cloning, use hard-link preservation as the default unlink/replacement fallback, with its weaker semantics visible in inspection output. Label it as retained-inode recovery, not an immutable historical version: another hard link or open writer can change it. Never use a hard link for in-place-write backups or scoped checkpoints. Do not treat `st_nlink == 1` as proof of immutability, and never chmod/chown a linked backup to secure it.

For operations requiring frozen contents without cloning, use a bounded independent copy or refuse protected execution. Capability failures may select a fallback; permission, I/O, and durability failures must not be casually reclassified as “unsupported.” Report weakened preservation explicitly. Never silently start an enormous byte copy.

Preserve raw pathname/symlink bytes, modes, relevant timestamps, xattrs/resource forks, ACLs, and supported flags to the extent available without elevation. Apple clones have ownership, set-ID, and ACL exceptions; account for them rather than assuming cloning exactly reproduces metadata. Report fidelity limitations. Do not promise identical inode numbers or kernel-managed timestamps.

Restore directory metadata after restoring children. Preserve known hard-link relationships within a captured set where safe; do not modify outside-set aliases to restore historical contents. A fresh independent version plus a clear fidelity notice is preferable to overwriting unrelated live data.

## 8. Storage, durability, and recovery

Keep the catalog under `~/.local/share/ish/undo`, with a versioned format, transaction metadata/journals, object storage, and a small registry of volume-local stores. Private directories protect stored data; use opaque object identifiers rather than user paths as object-store names.

Hard links and clones require suitable same-filesystem placement. Use the home store for its filesystem; provide explicit registration of a private store on other filesystems. Do not scatter hidden stores through arbitrary projects. Validate ownership, identity, location, and capabilities. A disconnected volume is unavailable—not evidence that its objects were deleted.

Keep enough transaction/object metadata with off-home payloads to reconcile interrupted writes and rebuild catalog associations. Do not pretend the central catalog and another filesystem commit atomically together.

Use a compact, versioned, length-framed journal with integrity checks, lossless paths, operation identity, saved-version references, and outcomes. Distinguish prepared operations from actual successful mutations. Deduplication must respect object/version identity and pathname reuse, not merely a path or command hash.

The ordering invariant is: **the saved object and information required to recover it exist recoverably before destructive mutation is permitted**. Implement appropriate file/directory synchronization, and propagate failures. Use bounded preparation batches to amortize durability costs during recursive removal rather than an unnecessary global flush per entry.

Recover valid records after a torn tail; preserve ambiguous prepared operations and orphan candidates for reconciliation. Recovery must not invent success, silently delete the only saved version, or automatically overwrite live files. Undo/redo themselves use this machinery and must be restartable after interruption.

Process-crash recovery is mandatory. Document the implemented power-loss durability contract accurately, including filesystem limitations; do not equate killing a process with testing power failure. Keep locking and recovery local and simple—no resident coordinator or database service.

## 9. Conditional undo and redo

Build a replay plan against current filesystem state before applying it. Default behavior must preserve newer data:

- Restore a deletion only into an unoccupied destination.
- Remove a created object only when its current state matches the recorded post-state.
- Replace modified content only when the expected post-state still matches.
- Reverse a rename only when the relevant namespace identities remain valid.

Do not use size/mtime alone as proof of equal contents. Compare against a frozen post-image, or use equivalent verified evidence, when replacement/removal requires it. Deletion capture must not hash a huge file merely to record an expected absent destination.

Use descriptor-relative operations and atomic no-clobber publication where available. Revalidate before mutation and preserve displaced objects. Filesystem races with unrelated writers are not solved by advisory locks; detected uncertainty becomes a conflict, not permission to overwrite.

Normal undo/redo never discard conflicting current data. `--force` means **save the conflicting current state first**, then proceed. Only explicit retention management permanently discards historical versions.

Restore a frozen object through a new clone/copy rather than exposing the sole immutable stored version as a live writable file. Maintain logical object/version references across identity changes introduced by replay. For linked-inode recovery, preserve and display the weaker contract instead of pretending immutability appeared during restoration.

Redo applies recorded filesystem state, not the original command. Keep per-action outcomes so retries, selective restores, partial conflicts, and interrupted replay do not repeat already completed actions. Selective restoration must include or validate required parent/rename dependencies, not blindly replay arbitrary journal indices. No multi-file atomicity claim is required.

## 10. User interface and retention

Provide a small builtin interface:

```text
undo [id] [--dry-run] [--only <path>] [--force]
undo redo [id] [--dry-run] [--only <path>] [--force]
undo list
undo show [id]
undo diff [id]
undo run --scope <directory> -- <program> [arguments...]
undo gc
undo purge <id>
undo doctor
undo volume ...
```

Choose concise volume registration/listing syntax and document it. Plain `undo` selects the latest eligible completed transaction from the current shell session, excluding its own active execution; explicit IDs address shared history across sessions. Resolve relative selection paths against the recorded transaction cwd. List/show expose partial coverage, linked versions, conflicts, and missing volumes without verbose warnings after every command.

Use existing rendering and completion patterns, not another TUI. Read-only inspection can participate in pipelines; define and test restrictions for recovery-mutating commands in pipeline/substitution contexts. Bound textual diffs and summarize binary/oversized content. Shell-owned work stays in-process.

Protected native mutations fail closed when required preservation fails. Ordinary opaque external programs are still allowed and must not be misleadingly labeled protected. `doctor` performs small, self-contained capability probes in registered stores and reports what actually succeeded.

Provide finite configurable retention, retained-size/entry budgets, minimum-free-space reserves, and a separate byte-copy fallback limit through existing configuration conventions. A cheap large clone must not inherit a tiny byte-copy limit, but retained blocks are not free. Distinguish logical retained size, allocated-size estimates, and transfer cost; do not claim exact exclusive CoW usage.

Run bounded maintenance at suitable command boundaries or on explicit `gc`, never per keystroke. Protect active, interrupted, and unavailable-volume transactions from inappropriate collection. Purge/GC must honor references and delete only objects owned by the store. Recovery commands should not recursively capture their own implementation operations.

## 11. Integration tests: broad, deterministic, and safe

Prefer cohesive filesystem integration suites and PTY scenarios over tests of getters, enum variants, or mocked method calls. Reuse ish’s existing PTY infrastructure. Add narrow unit tests only for genuinely tricky pure logic such as journal decoding or nonexecuting dangerous-target validation.

Every mutation test and benchmark must use a fresh private fixture root, with separate work, HOME, config, and store children. Explicitly configure child environments and cwd; do not change process-global environment around parallel tests. Verify observable filesystem contents and recovery results, not only exit codes or output substrings.

**Safety rules apply to implementation experiments as well as committed tests.** Never run destructive commands against real root/home/worktrees to test a guard. Exercise those decisions through a nonexecuting planner or a fixture-local logical root. Never require sudo, relaxed SIP, privileged containers, real-volume mounting/reformatting, actual disk exhaustion, or signals to unrelated processes. Cleanup is limited to validated harness-owned directories and must not follow symlinks. Kill only recorded child PIDs/process groups.

Cover these behaviors in a manageable set of scenario-driven suites:

| Suite | Required scenarios |
|---|---|
| Filesystem round trips | Recursive removal, move/overwrite, directory metadata, symlinks, raw-byte names, known hard-link groups, large and sparse files; undo/redo and restart. |
| Version isolation | Mutate via another hard link, an open descriptor, and a writable mapping after preservation. A claimed frozen version remains frozen; unsupported capture fails correctly; linked fallback is honestly weaker. |
| Shell execution | Aliases/globs/quoting, pipelines, command substitutions, successful/skipped/failed list branches, redirection ordering and failures, append and symlink targets, Ctrl+C, stop/resume. |
| Conditional replay | Same-path recreation, edits after capture and after undo, rename cycles, overlapping transactions/two shells, preserved force-conflicts, selective restoration, partial retry. |
| Scoped execution | An external fixture program creates, rewrites, renames, and deletes through child processes; failed execution still recovers; changes preserving size/mtime are not missed. |
| Failure recovery | Deterministic interruption around journal/preservation/mutation/replay boundaries, torn records, injected copy/write/sync/space failures, and recovery in a fresh process. |
| Stores and retention | Registration, missing/reappearing stores, cross-device paths, capability fallback, budgets, active-transaction protection, and GC/purge reference safety. |

Use a small Rust fixture executable where controlled descriptors, mappings, child processes, or failures are needed; do not add Python/Go runtime dependencies to the tests. Use readiness handshakes rather than timing sleeps. Fault injection belongs in test-only plumbing, not a public shell backdoor. Simulate ENOSPC/EXDEV/error boundaries rather than damaging the host environment.

## 12. Platform validation, performance, and handoff

Run the real macOS arm64 suite on APFS, including clone isolation and metadata behavior. Capability-specific assertions must establish that the intended backend actually ran. Unsupported capabilities get explicit, narrow skips or expected failure cases—not a blanket pass for the entire suite.

For Linux, reuse ish’s Dockerfile, toolchain, `musl-cargo`/Makefile conventions, and sibling epsh layout. Build and run Linux arm64 tests on the arm64 host. Update workspace test/release targets so the new crate is genuinely included. Preserve existing release/static-link verification where applicable.

Put Linux mutation fixtures in container-owned storage, not macOS bind mounts. Prefer read-only source mounts or copied source with container-owned build directories. Run permission-sensitive fixtures under an unprivileged UID, even when image construction/building uses root. Probe reflink support in the actual test location. Always exercise real hard-link/copy fallback paths when available; report native reflink validation separately if Docker’s filesystem cannot provide it.

Add bounded benchmarks using the existing benchmark machinery: an allocated moderately large file, a separately labeled sparse file, and a many-small-files tree. Compare ordinary removal, protected removal, undo/redo, and scoped checkpointing. Measure latency, entry counts, backend selections, and userspace bytes copied. Keep validation reads outside the timed capture interval. Do not infer zero data movement from sparse-file timings alone or enforce flaky absolute CI timing limits.

Demonstrate that clone/link deletion capture does not eagerly read/copy payload contents, and check that untouched startup, prompt, and completion paths do not acquire filesystem-walk overhead. Report real numbers with platform/backend details; do not invent speedups.

Update README/help with the coverage contract, native command options, recovery examples, retention, volume handling, and fallback limitations. Keep architectural documentation compact and progress bookkeeping minimal; do not create a collection of redundant status/design documents.

Finish by running formatting, linting, relevant epsh tests, the complete ish workspace/integration/PTY suites, and applicable release verification on both platforms. Resolve regressions rather than weakening existing assertions. Do not install/replace the user’s shell, modify login configuration, run pre-commit hooks, commit, or push.

The handoff should summarize changed areas, exact validation commands/results, benchmark results, new dependencies, and any genuinely unverified platform capability. Complete all required rootless functionality; the explicitly excluded OS-level capture backend is not unfinished work.

