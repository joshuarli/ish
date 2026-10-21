# Filesystem benchmark harness

`bench.sh` prepares, checks, measures, and removes bounded filesystem fixtures
under `.work/tmp`. Local P3 samples remain locked until the integrator confirms
feature readiness and available P2 gates.

## Prepare and remove fixtures

Run the fixture builder from the repository root:

```sh
benches/fs/bench.sh prepare "$PWD/.work/tmp/ish-fs-bench-check"
```

The helper refuses an existing fixture root. It requires the root to be a
direct child of `.work/tmp`. It prints the root device and inode. Save the
`root_identity` value for later verification and cleanup.

Check the fixture bytes before a later run:

```sh
benches/fs/bench.sh verify "$PWD/.work/tmp/ish-fs-bench-check" DEVICE:INODE
```

Remove the fixture after checking its ownership identity:

```sh
benches/fs/bench.sh cleanup "$PWD/.work/tmp/ish-fs-bench-check" DEVICE:INODE
```

If you prepare a cross-filesystem fixture, pass an existing mounted directory
below `.work/tmp` as the second argument. The helper requires a different
device and creates one owned child there. Pass the printed cross root and
identity to `verify`, `cleanup`, and `measure`. It never removes the mounted
parent.

Base fixture preparation writes about 307 MiB of real file data and creates a
1 GiB sparse file with three 1 KiB extents. A designated cross-device setup
adds a 64 MiB file and 16,384 4 KiB files on the main filesystem. Verification
reads every real data byte, checks the sparse holes and extents, checks
hardlink identity, and checks that the same-filesystem move fixture shares a
device with its destination. Verification makes the file cache warm. It does
not drop caches.

## Fixture shapes

| Workload | Fixture | Measured path |
| --- | --- | --- |
| Tiny operations | 96 files of 512 bytes | Repeated exact-target copies and moves, including an interactive shell run through a PTY |
| Flat tree | 4,096 files of 4 KiB | Many small files in one directory |
| Nested tree | 8 directory levels, 32 files of 256 KiB per level | One large nested subtree |
| Deep tree | 96 directory levels, one 1 KiB file per level | Traversal with a tight descriptor limit |
| Wide tree | 64 directories with 64 files of 4 KiB each | Traversal with a tight descriptor and address-space limit |
| Dry-run wide merge | 64 branches with 64 colliding 4 KiB files and a 16-level chain per branch; each destination branch also has an extra file | Skip collisions through a wide and deep merge without writing |
| Large copy | One 128 MiB patterned file | Real bytes, clone-capable copy when the filesystem and method counters confirm it, and buffered fallback |
| Sparse copy | One 1 GiB logical file with three written extents | Sparse extent copy and trailing-hole preservation |
| Same-filesystem move | A nested directory with an 8 MiB leaf | Rename path with a non-empty descendant tree |
| Cross-filesystem move | One 64 MiB patterned file | Only available with an explicitly designated different-device mount below `.work/tmp` |
| Cross-filesystem flat move | 1,024, 4,096, or 16,384 files of 4 KiB | Many small files moved to an explicitly designated different-device mount |
| Merge | Eight branches with 32 MiB of new files and 2 MiB of existing files | Explicit merge with nonconflicting subtrees |
| Hardlinks | 18 names for one 4 KiB inode and 256 ordinary 4 KiB files | Invocation-wide hardlink preservation mixed with ordinary files |
| Progress | 4,096-file wide tree, 16 MiB | Actual PTY runs. The enabled run must show a terminal progress line. |

Fixture validation proves that the inputs exist and have the stated shape. It
does not claim that an engine or utility copied them correctly. The sample
runner creates a unique destination for every side and run. A separate verifier
checks complete output bytes and directory entries after the post-sample load
check. It checks sparse allocation and hole bytes for engine and builtin
outputs, hardlink identity for ish, and that the same-filesystem rename reports
one discovered and completed entry. Native sparse copies are checked for
length and bytes; allocated extent preservation is utility-specific. P2 parity
tests remain the correctness gate for measured implementations. Move
samples also verify that every source path was retired, and the PTY runner reads
the shell's exit status after every builtin command.

## Check host capabilities

Print the host, filesystem, and native utility labels:

```sh
benches/fs/bench.sh host "$PWD/.work/tmp/ish-fs-bench-check"
```

The command labels GNU coreutils `cp --reflink=auto`, Apple `cp -c`, BusyBox,
and unclassified utilities separately. It does not infer filesystem clone
support from the utility name. BusyBox `cp` remains a plain native utility
comparison; it is never labeled GNU or clone-aware. No utility is treated as
proof that the filesystem supports cloning.

## Run the matrix

Set `ISH_FS_BENCH_FEATURES_READY=1` only after the integrator confirms feature
readiness and the available local P2 gates. Native parity cases that need an
unavailable host or utility may remain blocked; this flag does not complete
those cases or establish whole-campaign acceptance. Samples collected while
they are blocked are local evidence only. Do not use them to accept timing or
guide performance tuning until the remaining parity gates pass. The harness
builds its release test runner only after checking the feature gate and host
memory/disk capacity.

```sh
ISH_FS_BENCH_FEATURES_READY=1 benches/fs/bench.sh measure \
    "$PWD/.work/tmp/ish-fs-bench-check" DEVICE:INODE all
```

Pass an explicitly prepared different-device fixture to enable cross-device
move samples:

Replace `CROSS_ROOT` and `CROSS_DEVICE:INODE` with the printed `cross_root` and
`cross_identity` values from `prepare`.

```sh
ISH_FS_BENCH_FEATURES_READY=1 benches/fs/bench.sh measure \
    "$PWD/.work/tmp/ish-fs-bench-check" DEVICE:INODE all \
    "$CROSS_ROOT" CROSS_DEVICE:INODE
```

Use one workload name instead of `all` for a focused matrix. Names are
`tiny-copy`, `tiny-move`, `flat`, `nested`, `deep`, `wide`, `large`, `clone`,
`sparse`, `hardlinks`, `rename`, `cross`, `cross-flat-1k`, `cross-flat-4k`,
`cross-flat-16k`, `merge`, `dry-run-wide`, `progress-on`, and `progress-off`.

Every workload collects five accepted alternating runs for the engine and
actual interactive builtin. Copy workloads include the available host `cp`
side, and move workloads include the available host `mv` side. `dry-run-wide`
has no native utility side because `cp` and `mv` do not provide a matching
dry-run operation. GNU clone-aware `cp` and Apple `cp -c` are additional sides
for large, clone, and sparse workloads when the utility supports them.
Cross-device workloads are skipped unless the designated root has a different
device. Each sample prepares its unique inputs and destination before the
one-attempt readiness check. A timed sample passes that check immediately
before execution and `perf-gate-ready -1` immediately after. Overlapping samples
are discarded. After three busy checks, measurement stops and reports
`perf gate deferred: host busy`. Dry-run samples reuse their read-only source
and destination fixture. Other samples prepare their paths before timing. The
runner never drops caches, reports warm-cache samples only, and verifies
outputs after each post-sample check.

Each accepted sample prints one `fs_bench_sample` record. It labels the elapsed
scope: engine request calls, interactive command responses, or native utility
calls. The interactive side also reports startup and full-session elapsed time;
its status queries are included in full-session time and CPU use, but excluded
from the command-response interval. The engine side reports the
selected Clone, Native, and Buffered file counts, logical bytes cloned, bytes
streamed, file outcomes, and `Outcome.metrics`. `transfer_elapsed` includes
concurrent destination synchronization; `destination_sync_work` is summed sync
call time and can overlap; `source_retirement_elapsed` measures retirement after
the transfer receipt. Do not add these phase fields to estimate end-to-end
time. The actual PTY builtin reports startup, command-response, end-to-end,
CPU, RSS, command statuses, and verified output; the current builtin interface
does not expose its internal `Outcome` counters. Native utility physical I/O is
unknown.

The release test-runner build gets a separate readiness-gated wall and CPU
record. Each sample includes logical input size, process CPU, peak RSS, ish
binary size, requested jobs, the production transfer buffer and work queue
capacity, event delivery mode, record memory budget, descriptor budgets, and
process soft limits. It reports inline event delivery for direct engine calls;
the PTY adapter does not expose its event queue capacity. Native utility queue
and transfer-buffer fields are marked not applicable. Descriptor high water is
unobserved because the engine does not expose that counter. Deep and wide runs
use a 64-descriptor and 1 GiB address-space soft limit, subject to lower host
limits. Results are saved under `.work/tmp` and summarize median/range for
elapsed and CPU time, with minimum CPU time available for compute gate decisions.
The orchestrator continues after discarded samples until each side has five
accepted runs or three busy readiness checks defer measurement.

Native comparison labels preserve important mismatches. The engine requests
durable move completion; native `mv` durability is unverified. Native `cp`
preservation is utility-specific. Compare only when output mapping and required
preservation match, and retain the label when they do not.

## Measurement gate

The local Linux exploratory samples are in [results-linux.md](results-linux.md).

The host used to prepare this harness is Linux x86_64. Its `cp` and `mv` are
BusyBox. It has no GNU clone-aware `cp` and is not a macOS host. No successful
filesystem clone or genuine cross-filesystem move has been measured. A mounted
cross-device root was not designated, so the cross-device comparison is
unavailable. Those comparisons remain acceptance blockers until a supported
host and designated filesystem provide the required capabilities.
