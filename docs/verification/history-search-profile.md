# History search profile

The original baseline is the reviewed master revision `fb5797e`.

The baseline `perf` profile placed 76.8% of counted samples in `History::fuzzy_search_subset_into_in_dir`. The ASCII matcher showed a repeated forward scan before ranking. The profile lost about 21% of samples under host load, so it locates work but cannot support a speed claim.

The candidate passed the earliest matched-byte endpoint from the ASCII subsequence filter to the detailed matcher, removing its second forward scan. Differential tests passed. The candidate was reverted after these optimized benchmark runs showed no measurable win.

The runs used Rustybench with the optimized bench profile on Rust 1.100.0-nightly for `x86_64-unknown-linux-musl`. Each of five pairs ran the baseline and then the candidate. The `gco` and deep-directory benchmarks used five samples of 200 iterations per run. The pager trace used five samples of 50 iterations. Every timed run passed `perf-gate-ready` before and after measurement.

| Workload | Baseline median and range | Candidate median and range | Allocations per iteration |
| --- | ---: | ---: | ---: |
| `history_fuzzy_search_into_45k` (`gco`, root directory) | 784,692 ns (775,693–846,767) | 771,848 ns (770,996–807,257) | 1 allocation, 8,081 B on both sides |
| `history_fuzzy_search_in_dir_45k` (`g`, deep directory) | 3,386,178 ns (3,333,723–3,675,550) | 3,484,553 ns (3,379,572–3,705,552) | 1 allocation, 8,081 B on both sides |
| `history_search_trace` (sequential queries and pager rendering) | 4,529,065 ns (4,450,173–5,191,830) | 4,408,747 ns (4,315,999–4,862,065) | 4 allocations, 32,324–32,346 B across runs |

All three baseline and candidate ranges overlap. Candidate medians moved by -1.6% for `gco`, +2.9% in the deep directory, and -2.7% for the rendered trace. These differences are within run-to-run variation. The search allocation count and bytes did not change, so the candidate was not retained.

## Retained changes

The initial verification revision `ee1a2c1` produces a byte-for-byte identical static,
stripped release to the baseline: 1,229,016 bytes. Both builds use `make release`
with the pinned toolchain and musl target.

Five interleaved PTY pairs load 45,000 history entries, search with Ctrl+R and
`gco`, cancel the pager, verify `printf` output, and exit successfully.
GNU time records the shell's peak resident memory in KiB:

| Source | Minimum | Median | Maximum |
| --- | ---: | ---: | ---: |
| Baseline | 11,696 | 12,072 | 13,088 |
| Initial verification revision | 11,944 | 12,256 | 12,504 |

The ranges overlap, and the retained changes pass the 2× memory ceiling.

After the descriptor and fork stdout corrections, the same static release
checks pass at 1,229,464 bytes. Five new interleaved PTY pairs give these peak
memory results:

| Source | Minimum | Median | Maximum |
| --- | ---: | ---: | ---: |
| Baseline | 11,440 | 12,248 | 12,584 |
| Final corrections | 11,864 | 12,080 | 12,556 |

This revision also passes the 2× memory ceiling. These size and memory checks
establish no latency improvement.

## Fork cost check

Five interleaved PTY pairs each execute 2,000 command substitutions in 100
batches of 20 `printf '<%s>' "$(printf value)"` commands. Every batch must produce
20 `<value>` fields before the next prompt, and every shell must exit with
status 0. A separate syscall trace confirms one evaluator fork per substitution.

`wait4` records cumulative user and system CPU for the shell, its waited
children, and the GNU time wrapper, including startup. Each measured run passed
`perf-gate-ready` before and `perf-gate-ready -1` immediately afterward.

| Source | Minimum CPU s | Median CPU s | Maximum CPU s |
| --- | ---: | ---: | ---: |
| Baseline | 0.125565 | 0.129066 | 0.146971 |
| Final corrections | 0.125243 | 0.130484 | 0.138105 |

The ranges overlap and pass the 2× CPU ceiling. This process-creation workload
establishes no measurable CPU-cost change and makes no latency claim.

## Search ranking before materialization

The next profile targeted `history_search_trace` on master `0d62fe7`. A
five-second `sample` of the benchmark binary took 4,186 samples: 81.8% in
`History::search_limited`, 14.5% in `find_substring_icase_ascii_bytes`, and
2.7% in `memmove`. The pager cache and row rendering accounted for five
samples. The trace runs four searches over 45k entries (empty, `g`, `gc`,
`gco`), so the ranking scan, not the pager, was the cost.

`FuzzyMatch` is 272 bytes because every match carries 32 positions. The search
built one for each matching candidate before checking the 200-result limit, so
the empty query wrote about 12 MB of positions it discarded. Candidates now
rank by match tier only. Positions are built for the results that survive the
limit. The ranking entry carries the candidate index in the same vector, so
the search still makes one allocation.

A 26-bit ASCII presence mask then rejects a command that lacks a letter the
query needs. The mask is computed once when the command is stored and reuses
padding in `EntryUsage`, so the entry layout does not grow. It applies only
when both the query and the command are ASCII; Unicode case folding can map a
non-ASCII character onto an ASCII letter.

The mask adds one pass over each new command at load. The paired runs showed
no consistent change in `startup_warm` or `startup_cold`; their spread was
larger than any difference.

Three interleaved pairs ran the full suite on macOS aarch64 in worktrees at
`0d62fe7` and `4c01330`:

    rustybench baseline --root <worktree> --baseline <json> -- cargo bench --bench bench
    rustybench diff <baseline.json> <candidate.json>

`perf-gate-ready` returned 0 before each pair but reported a hot process tree
from an unrelated Rust build, and the load average stayed between 9 and 11.
The ratios, not the absolute times, support the change.

| Workload | Baseline median (range) | Candidate median (range) | Change |
| --- | ---: | ---: | ---: |
| `history_fuzzy_search_in_dir_45k` | 648,167 ns (609,146–757,208) | 393,500 ns (387,937–421,604) | -39.3% |
| `history_fuzzy_search_into_45k` | 674,563 ns (598,312–786,521) | 322,604 ns (299,813–324,000) | -52.2% |
| `history_search_trace` | 2,494,000 ns (2,325,416–2,830,333) | 1,247,250 ns (1,154,979–1,257,874) | -50.0% |

Each search still makes one allocation. The ranking vector grew from 6,432 B
to 8,040 B because it now carries the candidate index; the trace grew from
25,728 B to 32,160 B with four searches. The candidate range stayed below the
baseline range for each workload.
