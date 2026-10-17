# History search profile

The baseline is the reviewed master revision `fb5797e`.

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

After the descriptor correction in `b120b0f`, the same static release checks
pass at 1,228,536 bytes. Five new interleaved PTY pairs give these peak memory
results:

| Source | Minimum | Median | Maximum |
| --- | ---: | ---: | ---: |
| Baseline | 12,028 | 12,168 | 12,472 |
| Descriptor correction | 11,924 | 12,096 | 12,328 |

This revision also passes the 2× memory ceiling. These size and memory checks
establish no latency improvement.
