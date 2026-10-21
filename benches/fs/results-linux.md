# Linux exploratory filesystem samples

## Scope

These samples describe the final Linux source snapshot on one Linux x86_64 host
with ext4. They are local exploratory evidence. They do not establish P3
acceptance or support performance tuning.

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

The candidate `ish` binary was 2,196,344 bytes. Engine samples used a 262,144
byte transfer buffer, a 256-entry work queue, a 67,108,864 byte record memory
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

## Unavailable comparisons

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
