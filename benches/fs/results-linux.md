# Linux exploratory filesystem samples

## Original campaign historical snapshot

These samples preserve the original campaign snapshot on one Linux x86_64 host
with ext4. They are historical exploratory evidence and do not describe the
repaired before/after comparison below. They do not establish P3 acceptance or
support performance tuning.

The fixture verifier read all input bytes before sampling, so the file cache was
warm. The runner collected five accepted interleaved samples per available
side. Every accepted sample passed `perf-gate-ready` immediately before and
`perf-gate-ready -1` immediately after execution. The runner verified every
output. It recorded no overlapping samples.

The engine column measures direct engine request calls. The builtin column
measures command response time in an interactive PTY. Builtin CPU includes the
whole shell session and status queries. The native column measures BusyBox
utility calls, including child CPU. The scopes differ, so the table supports no
cross-side ranking.

Each time cell gives the median and `[minimum, maximum]` in seconds. CPU uses
the same five accepted samples. The builtin elapsed time excludes status query
responses while builtin CPU includes them.

## Elapsed and CPU samples

| Workload | Engine elapsed; CPU | Builtin elapsed; CPU | BusyBox utility elapsed; CPU |
| --- | --- | --- | --- |
| Tiny copy | 0.002382014 [0.002361776, 0.002600299]; 0.002348 [0.002327, 0.002522] | 0.052094687 [0.052003759, 0.104064651]; 0.077440 [0.076296, 0.079969] | cp: 0.003633356 [0.003403799, 0.004089595]; 0.003654 [0.003434, 0.004124] |
| Tiny move | 0.023433456 [0.023090393, 0.028083047]; 0.001218 [0.001202, 0.001294] | 0.086804714 [0.084455042, 0.136965799]; 0.091009 [0.089441, 0.094049] | mv: 0.003331582 [0.003038674, 0.003695244]; 0.003361 [0.003069, 0.003715] |
| Rename | 0.001137485 [0.000734799, 0.001314392]; 0.000086 [0.000084, 0.000091] | 0.003327815 [0.003247793, 0.003454886]; 0.006288 [0.006164, 0.006371] | mv: 0.000146348 [0.000126250, 0.000154884]; 0.000151 [0.000131, 0.000159] |
| Flat | 0.112548901 [0.112003693, 0.113736802]; 0.170996 [0.126132, 0.174814] | 0.088581819 [0.088480245, 0.113437052]; 0.155409 [0.144857, 0.190928] | cp: 0.038300126 [0.037637734, 0.039340997]; 0.038123 [0.037569, 0.039261] |
| Nested | 0.028992238 [0.028895965, 0.053920128]; 0.031921 [0.031302, 0.035054] | 0.030753271 [0.030643361, 0.082442172]; 0.041251 [0.039753, 0.053769] | cp: 0.010846627 [0.010758079, 0.012146811]; 0.010824 [0.010718, 0.012113] |
| Deep | 0.786757950 [0.781445027, 0.834034584]; 0.798422 [0.794543, 0.845488] | 0.813791960 [0.801906917, 0.820656387]; 0.837909 [0.820186, 0.852098] | cp failed at depth 59 with `EMFILE` under the 64-descriptor limit; no timed sample |
| Wide | 0.286527315 [0.263331672, 0.310459572]; 0.340390 [0.336003, 0.347886] | 0.398765235 [0.323714272, 0.407776678]; 0.492943 [0.411679, 0.525395] | cp: 0.027259199 [0.026639168, 0.027453679]; 0.026755 [0.026598, 0.027411] |
| Large | 0.034473732 [0.022778901, 0.179746895]; 0.024735 [0.022689, 0.025577] | 0.042922854 [0.026442635, 0.053596453]; 0.034767 [0.033294, 0.035265] | cp: 0.019017280 [0.018639220, 0.020239927]; 0.018992 [0.018601, 0.020197] |
| Sparse | 0.000287888 [0.000258072, 0.000303678]; 0.000290 [0.000260, 0.000297] | 0.013590620 [0.002183747, 0.026705685]; 0.007597 [0.007084, 0.008721] | cp: 0.140446806 [0.137165639, 0.143470562]; 0.140220 [0.136241, 0.143175] |
| Hardlinks | 0.025369441 [0.025349093, 0.025396142]; 0.000795 [0.000762, 0.001015] | 0.103583004 [0.097304653, 0.114159668]; 0.009133 [0.008822, 0.010969] | cp: 0.000208808 [0.000152600, 0.000331501]; 0.000214 [0.000155, 0.000335] |
| Merge | 0.031663583 [0.027403233, 0.042672338]; 0.028273 [0.026640, 0.029878] | 0.082688159 [0.080302740, 0.097551844]; 0.039426 [0.038884, 0.045780] | cp: 0.009147803 [0.008953914, 0.011298588]; 0.008932 [0.008392, 0.009212] |
| Clone | 0.099067940 [0.031317354, 0.351809974]; 0.023667 [0.022686, 0.024360] | 0.037613058 [0.024907933, 0.074947505]; 0.031772 [0.031416, 0.032239] | cp: 0.018679588 [0.018126946, 0.020765618]; 0.018653 [0.018089, 0.020684] |
| Progress on | 0.292835523 [0.265028051, 0.440786453]; 0.367294 [0.356099, 0.393088] | 0.352765902 [0.323075646, 0.468457547]; 0.436018 [0.402459, 0.438593] | cp: 0.028388598 [0.027818785, 0.041163276]; 0.028289 [0.027691, 0.029289] |
| Progress off | 0.315551123 [0.288948443, 0.341987165]; 0.367752 [0.359038, 0.413312] | 0.317218076 [0.314882601, 0.345153452]; 0.406111 [0.392131, 0.434110] | cp: 0.029763135 [0.028706724, 0.030052987]; 0.029597 [0.028610, 0.029720] |

The progress workloads both copied the 4,096-file, 16 MiB wide fixture. The
interactive PTY showed a progress line in all five `progress-on` samples and no
progress line in all five `progress-off` samples. The earlier 128 MiB progress
samples are excluded because they did not show progress.

## Engine methods and transferred bytes

The method counts below describe selected engine methods per copied file.
`Native` means the engine selected its native copy path. The `Clone` count
records actual clone operations. The engine selected no Clone or Buffered
copies in these samples. Logical cloned bytes were zero throughout.

| Workload | Logical input bytes | Clone / Native / Buffered files | Discovered / completed | Streamed bytes |
| --- | ---: | ---: | ---: | ---: |
| Tiny copy | 16,384 | 0 / 32 / 0 | 32 / 32 | 16,384 |
| Tiny move | 16,384 | 0 / 0 / 0 | 32 / 32 | 0 |
| Rename | 8,388,608 | 0 / 0 / 0 | 1 / 1 | 0 |
| Flat | 16,777,216 | 0 / 4,096 / 0 | 4,097 / 4,097 | 16,777,216 |
| Nested | 67,108,864 | 0 / 256 / 0 | 265 / 265 | 67,108,864 |
| Deep | 98,304 | 0 / 96 / 0 | 193 / 193 | 98,304 |
| Wide | 16,777,216 | 0 / 4,096 / 0 | 4,161 / 4,161 | 16,777,216 |
| Large | 134,217,728 | 0 / 1 / 0 | 1 / 1 | 134,217,728 |
| Sparse | 1,073,741,824 | 0 / 1 / 0 | 1 / 1 | 12,288 |
| Hardlinks | 8,192 | 0 / 1 / 0 | 3 / 3 | 4,096 |
| Merge | 33,554,432 | 0 / 512 / 0 | 1,032 / 512 | 33,554,432 |
| Clone | 134,217,728 | 0 / 1 / 0 | 1 / 1 | 134,217,728 |
| Progress on | 16,777,216 | 0 / 4,096 / 0 | 4,161 / 4,161 | 16,777,216 |
| Progress off | 16,777,216 | 0 / 4,096 / 0 | 4,161 / 4,161 | 16,777,216 |

`transfer_elapsed` includes concurrent destination synchronization.
`destination_sync_work` sums sync-call durations, which may overlap.
`source_retirement_elapsed` starts after the transfer receipt. Do not add these
fields to estimate end-to-end time. The direct engine samples report each field.
The PTY and native utility interfaces do not expose these counters.

## Host and resource limits

The fixture used Linux x86_64, ext4, and device 66307. The fixture verifier
warmed the cache by reading all input data. The host reported 32 CPU cores, 91%
free memory, and 502 GiB free on the filesystem before the first measured
matrix. The runner recorded no discarded overlap.

The original campaign candidate `ish` binary was 2,196,344 bytes. It was built
before baseline commit `11cef310` and its minor error-path patch. A fresh
release build from the actual `11cef310` baseline is 2,196,792 bytes. Engine
samples used a 262,144 byte transfer buffer, a 256-entry work queue, a
67,108,864 byte record memory
budget, and descriptor budgets of 256 maximum, 32 reserved, and 16 minimum.
Deep and wide workloads used a 64-descriptor soft limit and a 1 GiB address
space limit. Other workloads used a 1,024-descriptor limit and unlimited
address space. Progress-on and progress-off used the same wide tree with the
default limits. The sample did not observe descriptor high water.

Across accepted samples, the `peak_rss_bytes` field ranged from 1,069,056 to
1,892,352 bytes for direct engine runs, 1,576,960 to 5,283,840 bytes for PTY
builtin runs, and 847,872 to 3,022,848 bytes for native utilities. This field
uses the maximum of the runner and waited-child `getrusage` high-water values.
It is not a polled instantaneous peak.

## Unavailable comparisons in original campaign

The host's `/bin/cp` and `/bin/mv` are BusyBox utilities. BusyBox `cp` is not a
clone-aware comparison utility. The host was not macOS, the filesystem clone
capability was not probed, and no successful clone was observed. The
clone-workload engine selected Native for its one file. The native move samples
do not verify the engine's durability guarantee.

No distinct-device directory was designated below `.work/tmp`, so the
cross-filesystem move workload was unavailable. A prior BusyBox `cp` attempt
under the same 64-descriptor soft limit failed with `EMFILE` at depth 59. The
final matrix has no deep native timing sample. The engine and PTY deep samples
remain exploratory evidence without a matched three-side comparison.

## Repaired Linux comparison

Before is baseline commit `11cef310a846dcbbef1aedb67c3103cbbd530ba1`; after is
the frozen, uncommitted repaired source snapshot. Both used Linux x86_64 with
an ext4 fixture root (device 66307). Cross-device destinations were separate
tmpfs mounts (devices 35 and 36) created inside a private user and mount
namespace. Source and destination devices differed; actual rename returned EXDEV.
The cross-filesystem traversal option permits nested mounts and does not force
EXDEV.

The fixture helper verified every input byte before sampling, warming the cache.
Fixture setup writes were not individually synced and make no durability claim.
Except for the explicitly marked single 16k builtin result, each measured side
had five interleaved before/after runs. Every accepted sample passed
`perf-gate-ready` immediately before and `perf-gate-ready -1` immediately after,
and passed output verification. No accepted sample overlapped host load. Direct
engine CPU measures the request operation; builtin CPU covers the whole PTY
session, while its elapsed time covers command responses; BusyBox CPU includes
the utility child. These interfaces do not have identical scopes, so compare
before/after within each interface only.

The table reports CPU seconds as median `[minimum, maximum]`, before → after.
Every cell has five accepted samples except where marked. Deep BusyBox `cp`
failed on both versions with `EMFILE` at depth 59 under the workload's 64
descriptor limit.

| Workload | Engine CPU | Builtin CPU | BusyBox utility CPU |
| --- | --- | --- | --- |
| Tiny copy, 32 × 512 B | 0.002799 [0.002760, 0.004939] → 0.006160 [0.006094, 0.006262] | 0.111002 [0.092495, 0.120328] → 0.175803 [0.162965, 0.179869] | cp 0.003559 [0.003472, 0.003696] → 0.003510 [0.003398, 0.003728] |
| Tiny move, 32 × 512 B | 0.001330 [0.001312, 0.001405] → 0.001343 [0.001326, 0.001380] | 0.111257 [0.104692, 0.114555] → 0.107748 [0.103888, 0.109882] | mv 0.003196 [0.003030, 0.003555] → 0.003162 [0.003018, 0.003301] |
| Whole-directory rename, 8 MiB payload | 0.000097 [0.000095, 0.000100] → 0.000093 [0.000090, 0.000180] | 0.008363 [0.007475, 0.009659] → 0.007185 [0.006613, 0.008440] | mv 0.000135 [0.000119, 0.000221] → 0.000137 [0.000126, 0.000164] |
| Wide, 4,096 files / 16 MiB | 0.360678 [0.352058, 0.448751] → 0.408705 [0.334669, 0.420858] | 0.437873 [0.364990, 0.486664] → 0.356620 [0.343212, 0.436063] | cp 0.029001 [0.027498, 0.030772] → 0.030121 [0.028666, 0.031521] |
| Deep, 96 files / 96 KiB | 0.820345 [0.797429, 0.821947] → 0.303836 [0.294456, 0.322758] | 0.838167 [0.791870, 0.865093] → 0.303297 [0.302086, 0.393618] | cp: `EMFILE` on both versions |
| Hardlink group plus ordinary files, 274 names | 0.010288 [0.010022, 0.019579] → 0.022315 [0.021917, 0.022762] | 0.021009 [0.019544, 0.022467] → 0.032927 [0.031984, 0.035386] | cp 0.002738 [0.002635, 0.002939] → 0.002817 [0.002616, 0.002845] |
| Dry run, 64 branches / 20 MiB input | 0.078566 [0.077790, 0.098706] → 0.045913 [0.044641, 0.060236] | 0.204548 [0.194005, 0.241141] → 0.130923 [0.122226, 0.136788] | not applicable |
| Cross flat, 1,024 files / 4 MiB | 1.722366 [1.630759, 1.870485] → 0.230708 [0.189096, 0.257706] | 1.253761 [1.207061, 1.589326] → 0.210612 [0.204808, 0.339772] | mv 0.010271 [0.009458, 0.012143] → 0.009906 [0.009659, 0.011624] |
| Cross flat, 4,096 files / 16 MiB | 21.157331 [20.606647, 22.018969] → 0.856117 [0.812355, 0.898678] | 18.014676 [17.559632, 18.548841] → 0.888138 [0.886525, 1.185642] | mv 0.044182 [0.039704, 0.049238] → 0.042277 [0.040510, 0.044699] |
| Cross flat, 16,384 files / 64 MiB | baseline timed out → 3.837106 [3.798347, 4.029907] | baseline timed out → 4.188633 (one accepted sample) | mv 0.180222 [0.178781, 0.185178] → 0.181343 [0.179197, 0.197923] |

Cross-flat wall elapsed seconds are medians `[minimum, maximum]`, before →
after:

| Workload | Engine | Builtin | BusyBox `mv` |
| --- | --- | --- | --- |
| 1k | 13.676 [11.453, 14.685] → 6.704 [2.509, 11.854] | 2.658 [2.588, 10.523] → 1.657 [1.602, 9.249] | 0.0103 [0.0095, 0.0122] → 0.0099 [0.0097, 0.0117] |
| 4k | 26.958 [26.405, 59.909] → 6.626 [6.384, 7.663] | 24.388 [22.951, 36.882] → 7.623 [6.418, 21.865] | 0.0446 [0.0402, 0.0497] → 0.0424 [0.0410, 0.0449] |
| 16k | timeout → 34.913 [26.090, 43.528] | timeout → 38.067 (one accepted sample) | 0.1862 [0.1846, 0.1918] → 0.1874 [0.1818, 0.2290] |

The before engine and builtin each stopped after one timeout with a passing
after-gate. A second candidate builtin attempt was terminated before it emitted
a sample record and its owned fixture was cleaned; it is not a result. No
same-side timeout was retried.

The engine used four jobs for unconstrained workloads and two for wide, deep,
and dry-run-wide; the constrained workloads used a 64-descriptor soft limit and
1 GiB address-space limit. Other samples used a 1,024-descriptor limit. Engine
configuration was a 256 KiB transfer buffer, 256-entry work queue, and 64 MiB
record-memory budget. The hardlink fixture has 18 names to one inode plus 256
ordinary files; engine and builtin output checks verified the group remained
linked and ordinary files remained separate. Dry-run-wide discovered 6,208
entries, skipped 5,120, completed none, failed none, and left the source and
destination unchanged. The whole-directory rename discovered and completed
one entry, confirming it did not enumerate the 8 MiB subtree.

For cross-flat, all engine samples used the buffered move path: 1,024, 4,096,
and 16,384 files plus one root directory were discovered and completed, with
4, 16, and 64 MiB streamed. Same-filesystem tiny-copy, wide, deep, and hardlink
fixtures selected Native (32, 4,096, 96, and 257 data copies respectively); no
Clone method succeeded.
Median engine source-retirement elapsed was 13.645 → 6.643 s at 1k and
26.859 → 6.386 s at 4k; the corresponding 16k candidate median was 33.896 s.
`transfer_elapsed`, `destination_sync_work`, and source-retirement timings are
phase measurements and must not be added. The indexed record-lookup regression
checks also reduced measured record visits from 8,256 / 32,896 at 128 / 256
records to 1,794 / 4,098. An untimed baseline `strace -f -c` profile of the
274-entry hardlink sample reported 601 futex calls, 79.87% of traced syscall
time; tracing overhead excludes it from the timing table. Physical I/O was
unavailable to the runner.

Median engine phase seconds are listed separately; sync work sums calls and can
overlap transfer, so these are not additive totals:

| Files | Version | Transfer elapsed | Destination sync work | Source retirement elapsed |
| --- | --- | --- | --- | --- |
| 1,024 | before | 0.030500 | 0.000115 | 13.645423 |
| 1,024 | after | 0.061499 | 0.000119 | 6.642580 |
| 4,096 | before | 0.098813 | 0.000427 | 26.858591 |
| 4,096 | after | 0.239977 | 0.000427 | 6.386377 |
| 16,384 | after | 1.015658 | 0.001412 | 33.895723 |

Cross-device retirement and deep traversal use less CPU in these samples;
tiny-move and isolated rename show no material CPU change. Tiny-copy and mixed
hardlink copies use more CPU, with nonoverlapping ranges: the extra output-version
validation and indexed coordination add small-operation cost. These results do
not establish a universal speedup. Wide-copy ranges overlap. The deterministic
concurrency test establishes that unrelated data phases overlap; this small
hardlink timing fixture does not show a throughput win.

Observed `getrusage` peak RSS across accepted samples ranged from 11,177,984 to
11,243,520 bytes before and 11,177,984 to 11,505,664 bytes after for engine and
builtin runs; native utility ranges were 11,243,520 bytes for `cp` and
11,177,984 to 11,505,664 bytes for `mv`. These are process high-water values,
not polled instantaneous peaks. The rebuilt baseline and candidate `ish`
binaries were 2,196,792 and 2,255,160 bytes, a 58,368-byte increase. Both
release test-runner builds passed readiness checks immediately before and
after. Baseline took 9.58 s elapsed / 9.04 s user / 0.75 s system; candidate
took 20.52 s / 38.16 s / 2.02 s. Different isolated targets and cache states
make these single-build figures noncomparative.

BusyBox `cp` and `mv` are context only: `cp` is not clone-aware, and these
utility samples do not verify the engine's durability guarantee. No comparison
across engine, PTY builtin, and native utility timings is claimed.

## Follow-up hardlink sample

This five-sample run measured the repaired working tree on Linux x86_64,
kernel 7.2.9-2-stable, Alpine musl, and ext4 (device 66307). The runtime source
identity is base `dfb615457ad4828216d9d8b8ea44711e8806b633` plus the binary diff
for `engine/mod.rs`, `engine/transfer.rs`, and `engine/dry_run.rs` with Git blob
ID `412adb0b15fad3b4f1d891db661e859d8664427b`. This identifies the measured
runtime patch; test and documentation edits do not affect the measurement.

The command was:

```sh
ISH_FS_BENCH_FEATURES_READY=1 benches/fs/bench.sh measure \
    "$PWD/.work/tmp/ish-fs-hardlink-final" 66307:66201313 hardlinks
```

All five samples per side passed `perf-gate-ready` immediately before and
`perf-gate-ready -1` immediately after, had no overlapping load, and passed
output verification. The fixture verifier had warmed the file cache. It held
18 names for one 4 KiB inode and 256 ordinary 4 KiB files (275 entries
including the root), totaling 1,052,672 logical bytes. The engine selected
Native for 257 data copies and preserved the hardlink group. The owned fixture
was removed after verifying its device and inode identity.

| Side and timing scope | Elapsed median [min, max] | Process CPU median [min, max] |
| --- | ---: | ---: |
| Direct engine request | 0.034146371 [0.033148481, 0.034698773] s | 0.021480 [0.020226, 0.021672] s |
| Interactive builtin command responses | 0.087715206 [0.085480991, 0.116500481] s | 0.030228 [0.029567, 0.033840] s |
| BusyBox `cp` utility calls | 0.002816974 [0.002203926, 0.003315274] s | 0.002719 [0.002187, 0.003317] s |

This is a descriptive sample of ordinary hardlink-group copying. It does not
measure replacement of aliased destinations or deferred blocking flags, and it
has no interleaved pre-repair baseline. The interface scopes and BusyBox
preservation/durability policy differ, so it supports no speedup claim.
