# fz integration validation

The baseline is ish `05f6810be0473d10692cfb7776fb4dee96c4c94f`.
The shared source is fz `8a91d565dff85caac4131a4bde90f5c685812590`.
Its final changes adjust whitespace; validation uses identical executable code.
[Picker reference](../fuzzy-pickers.md) defines the delivered behavior and
platform identity limits. [Raw results](fz-integration/) retain the measurements.
The final comparison uses `current-*` results with identical dependencies on
both sides. `accepted-*` and `delivered-*` retain earlier measurements against
`c30a151d6fd1a305a5ac362d21145e327bd639b3`, before the upstream dependency update.

## Runtime and semantic checks

Native macOS ARM64 and Linux x86_64 musl passed workspace Rust tests and Clippy
with warnings denied. Real interactive PTY checks cover fz pipelines,
substitutions, redirections, lookup, raw bytes, duplicate candidates, cancellation,
paste, queued input, process acceptance and TERM/KILL against owned children.
Native process tests include IPv4/IPv6, wildcard, shared/inherited listeners,
SO_REUSEPORT, socket closure and UDP exclusion. Injected backend tests exercise
batch rejection, stale identities, ownership and metadata changes, listener loss,
resource failure and interruption after a partial send. A numbered-redirection
regression failed before worker descriptor relocation and passes afterward.
Additional regressions verify queued substitutions inside whole-line builtins,
exactly one bracketed-paste cleanup, and the visible PID of an owned process
before accepting a signal. Picker rendering uses explicit carriage returns in
raw output mode so rows retain their PID columns.

The canonical fz suite passed on macOS with the pinned upstream fzy oracle
`34b88869d022e861da4846c4463aea3ddfb3ff30`. Linux native fz tests also passed.
Standalone matching, scores, byte framing and default key contracts remain
covered by the canonical suites. Embedded searches default to one worker.

Linux `make verify` passed the six 1,000-case semantic families, retained cases,
protocol checks, theorem axiom audit and all four compiled production mutants.
`make verify-refinement` passed production shift extraction and both axiom audits.
macOS passed the policy bridge and Lean build/audit. Its full expansion bridge
cannot create an invalid-UTF-8 filename on the native filesystem and is not
reported as passing.

The release binaries were built natively on macOS ARM64 and Linux x86_64 musl.
Linux ELF checks confirm static, stripped output without an interpreter or
NEEDED entries. Linux ARM64 passed a build-std cross-check. ARM64 Linux runtime
behavior was not exercised.

## History and picker cost

The runs use nightly-2026-09-15, optimized Cargo bench profiles, identical
corpora and one macOS ARM64 host with 18 cores. Five baseline/candidate pairs
alternate A then B. Each side uses five samples of 50 iterations.
`perf-gate-ready -1` passed immediately before and after every timed side.
The process-only measurement also passed both checks. Earlier busy-host
measurements are excluded from these latency tables.

The baseline adds only the two shared workloads in
[baseline-workloads.patch](fz-integration/baseline-workloads.patch).
Apply it to the baseline with `git apply --unidiff-zero`.
The existing 45k corpus and production search entry points are unchanged.
`history_editor_mixed_trace_45k` drives the actual Editor through append,
backspace, mid-query edits and paste with Unicode and long records in the corpus.
The construction case includes corpus creation, complete text copying and arena
construction. The startup fixtures retain their existing 2k-history workload.

Each cell is the median of five run medians, followed by their range.

| Workload | Baseline | Integrated |
|---|---:|---:|
| 45k `gco` query | 336.972 µs (299.274–350.336) | 335.112 µs (307.121–348.479) |
| 45k deep-directory query | 435.948 µs (422.767–456.734) | 426.440 µs (384.096–453.096) |
| Existing rendered search trace | 1.303 ms (1.148–1.352) | 1.252 ms (1.144–1.300) |
| Editor mixed trace | 13.713 ms (12.670–13.991) | 14.049 ms (12.829–14.677) |
| 45k corpus and search construction | 6.146 ms (5.489–6.292) | 6.325 ms (5.861–6.754) |
| Existing cold startup fixture | 708.941 µs (659.726–720.925) | 721.116 µs (615.058–787.818) |
| Existing warm startup fixture | 515.467 µs (411.232–536.746) | 504.804 µs (429.922–566.203) |

All ranges overlap. The mixed editor trace is 0.336 ms (2.5%) slower across its
eight key events and two paste events. That change is smaller than the observed
per-side run spread and adds no allocations. No measurable general history
speedup or regression is established. A separate sample profile places the work in
`History::search_limited`, including Unicode character-vector allocation,
rather than terminal rendering. That unmodified path limits the mixed trace.

Both revisions allocate once and 8,040 bytes per 45k search. The existing trace
uses four allocations and 32,160–32,182 bytes. The mixed Editor trace uses
90,036 allocations and about 3.91 MB, retaining the existing Unicode allocation
cost. Corpus construction uses 90,022 allocations and 8,620,221 bytes. These
counts include the timed complete-history work rather than hiding its copies.

A serial 512-process picker construction, query and render takes 43.442 µs
(42.511–46.063) across five runs, each with five samples of 200 iterations,
with 542 allocations and 76,510 bytes.
The timed work copies the complete fixture corpus, filters, ranks and produces
nonempty terminal bytes. There is no baseline pk implementation to compare.

## Resident memory, actual startup and release size

The PTY probe loads 45,000 saved commands into a fresh shell, opens Ctrl+R,
types `gco`, cancels, verifies printf output and exits with status 0. `wait4`
records the shell's peak RSS. Five interleaved baseline/candidate pairs include
all history load and import costs. The final batch uses the final release binary
and passes `perf-gate-ready -1` before and after the complete batch.

| Peak RSS | Baseline | Integrated |
|---|---:|---:|
| Median bytes | 21,807,104 | 19,988,480 |
| Range bytes | 21,790,720–23,642,112 | 19,955,712–21,757,952 |

The final batch shows no RSS regression. Residency varies between batches,
including earlier measurements with nearly equal medians. No general memory
reduction is claimed. The observations include complete history loading.

The final stripped macOS release grows from 1,897,408 to 2,030,368 bytes,
about 7.0%. Standalone fz's identically configured dist build grows from 386,192
to 386,448 bytes. The runtime fz dependency reuses existing rustix/libc versions
and adds no CLI, TUI, asynchronous or process-monitoring framework. Build-time
impact was not isolated from compiler cache and host load, so no build-speed
claim is made.

Final baseline startup is 32.801 ms (30.655–154.664), versus 31.235 ms
(28.948–41.571) integrated. Ctrl+R open, synchronize, query and render is
2.239 ms (2.211–2.835), versus 2.438 ms (2.422–2.476). The ranges overlap;
the median differences are smaller than the observed per-side spreads.
The baseline startup outlier remains reported. This establishes no startup or
whole-session search speedup. The isolated production search benchmarks above
separate CPU search cost from process startup, journal import and PTY scheduling.
Earlier ungated PTY timings are diagnostic only.
