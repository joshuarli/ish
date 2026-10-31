Prefix: ISH
Next: 65

## Accepted owner decisions

The linked decisions are accepted implementation contracts, with acceptance
criteria. Retain decision records after delivery and update the shipped behavior
references when implementation lands.

This table records accepted decisions, implementation dependencies, and
verification limits. Open implementation and acceptance work is listed below.

| Tickets | Accepted direction | Implementation dependency |
|---|---|---|
| [ISH-24](docs/rfc/ISH-24-builtin-allowlist.md) | Finite builtin policy; environment-only `set`; shared dispatch/lookup/completion rules | Implemented; focused Linux acceptance passed |
| [ISH-26](docs/rfc/ISH-26-denv-sourced-file-trust.md) | Retire implicit sourcing helpers; keep trusted `.env` assignment parsing | Implemented; both evaluators and rejection regressions passed |
| [ISH-15, ISH-52](docs/rfc/ISH-15-environment-and-shell-ownership.md) | Explicit environments; remove host environment mutation and unsupported safety comments | Implemented; Linux and ordinary-user macOS environment/history checks passed |
| [ISH-5](docs/rfc/ISH-5-foreground-job-outcomes.md) | Structured stopped-job handles; epsh owns every child wait | Implemented; Linux and native macOS PTY/process tests passed |
| [ISH-8](docs/rfc/ISH-8-callback-process-boundary.md) | Creator-process callbacks by default; explicit unsafe child opt-in | Implemented after ISH-5; Linux and native macOS callback boundary tests passed |
| [ISH-32](docs/rfc/ISH-32-capture-and-retire.md) | Shared journaled fdu quarantine and identity primitives | Linux x86_64 and ARM64 musl acceptance passed, including Btrfs FICLONE; ARM64 ran in an OrbStack VM; macOS under ISH-45 |
| [ISH-42](docs/rfc/ISH-42-release-delivery.md) | Default-branch releases, verified bytes, diagnostic panic aborts | Linux implementation verified; native macOS release build and panic diagnostic passed |
| [ISH-48](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor) | Private binary `Shell`; reusable library editor | Implemented; focused Linux editor and PTY acceptance passed |
| [ISH-45](docs/filesystem.md#macos-staging-permissions) | Keep filesystem operations in-process; macOS masks must preserve owner read/search | Implemented with explicit rejection; ordinary-user native gates passed; privileged ownership acceptance remains |

## Open implementation tickets

### ISH-64 · P2 · Resume interactive hot-path work from the history rewrite

- **Status:** Partially delivered. History ranking and the ASCII-letter filter landed in `e2fb0d4` and `4c01330`. Three interleaved macOS aarch64 pairs against `0d62fe7` moved `history_search_trace` by -46% to -56%, `history_fuzzy_search_into_45k` by -46% to -59%, and `history_fuzzy_search_in_dir_45k` by -31% to -48%. The search allocation count is unchanged; the transient ranking buffer grew by 1,608 B. `docs/verification/history-search-profile.md` records the profile and the per-run medians. Completion metadata lookup moved to `fstatat` against the opened directory handle in `927ba47`; a 200-match completion fell from 3.60 ms to 0.25 ms in interleaved binary runs.
- **Area:** Performance
- **Where:** `benches/bench.rs`, `src/history.rs`, `src/complete.rs`, `src/ls.rs`, `src/shell.rs`, `docs/verification/history-search-profile.md`
- **Problem:** `History` is 5.81 MB at 45k unique commands, down from 22.32 MB (129 B per entry, command text is 1.53 MB of it). The trace profile put 81.8% of samples in `History::search_limited`; the pager cache and rendering accounted for five samples. Every matching candidate built a 272-byte `FuzzyMatch` before the 200-result limit, and every candidate ran the byte matcher even when the command lacked a query letter. Completion rebuilt each matching entry's full path and re-walked it in `stat`. All are fixed.
- **Recommendation:** The two search leads are delivered. `startup_cold` is still dominated by the legacy migration path, which re-parses the text history and writes 2000 framed records per run. It is paid once per upgrade, so weigh any change against that. `cd_prompt_with_denv` costs roughly 8x the plain path; the owner deferred it, so do not take it without asking. Not worth redoing: `bbtree` (`docs/decisions.md`), a top-K `BinaryHeap` for history ranking, skipping the denv variable clone, and prefix-compressing the command arena.
- **Measurement:** `perf-gate-ready` can exit 0 while another workload saturates the host, so check `uptime` and `ps -Ao pcpu,comm -r` too. Work in two worktrees and compare with `rustybench baseline --root <wt> --baseline <json> -- cargo bench --bench bench` followed by `rustybench diff`, interleaving baseline and candidate. Repeated runs of the same binary spread 20–40 percent on a busy host, so trust paired ratios, not single runs. The allocator counters in the bench report are deterministic and stay usable when timing does not. For attribution, run the workload in a loop long enough to sample and use `sample <pid> 4 -file <out>`; its leaf-stack table put the trace cost in `History::search_limited`. `xctrace export` did not carry backtraces. Scratch probes used this session lived in `.work/tmp` and are disposable; rebuild them from the benchmark fixtures in `benches/bench.rs` rather than relying on them.
- **Fix with:** make-it-fast
- **Effort:** S (remaining)

### ISH-60 · P3 · Journaled retirement makes large flat EXDEV moves take minutes

- **Status:** Candidate parked at `e2f0f08` on `backlog-retirement-2026-10-23`; it is not integrated. Focused correctness, mount fixtures, and complete baseline/candidate 4k and 16k output verification passed. Timing proof is deferred because unrelated host builds overlapped samples.
- **Area:** Performance
- **Where:** `crates/ish-fs/src/engine/retirement.rs`, `crates/fdu-posix/src/quarantine.rs`, `benches/fs/results-linux.md`
- **Problem:** On Linux x86_64 ext4-to-tmpfs, the 16,384-file, 64 MiB EXDEV engine sample at source revision `0494a1e8c457ee69a2281a9f2fbb02e85036d4e6` took 300.993 seconds wall and 199.162 seconds CPU; source retirement accounted for 264.073 seconds. Five 4,096-file samples had a 43.331 second median wall time and 13.009 second median CPU, with wide wall-time spread. ARM64 Linux syscall profiles now identify repeated destination-root recovery scans. The parked candidate holds recovery ownership for flat destination roots, preserves namespace rechecks and per-entry synchronization, and passes focused tests; source-root scans remain. No valid timing comparison establishes its benefit. Its profiles, resource observations, and discarded timing attempts are recorded in `benches/fs/results-linux.md` on the parked branch.
- **Recommendation:** On a quiet host, collect five interleaved baseline/candidate 4,096-file samples with the same durable journal guarantees, readiness checks before and immediately after each sample, and full output/source-retirement verification. Reuse the retained correctness and resource evidence where applicable, and qualify the 1k and 16k observations separately. Integrate the candidate only if its measured benefit exceeds observed spread without weakening intent durability, source-version validation, recovery behavior, or destination receipts. Current result: `perf gate deferred: host busy`.
- **Fix with:** make-it-fast
- **Effort:** L (weeks)

### ISH-45 · P3 · Native macOS privileged ownership acceptance remains incomplete

- **Status:** The accepted restrictive-umask boundary is implemented. Ordinary-user macOS workspace tests, `make test`, and strict workspace Clippy passed. Only privileged ownership acceptance remains. Re-checked: `clone_declines_inherited_read_denial_before_creation` and `clone_declines_before_creation_when_owner_read_is_absent` still take the root branch only when `geteuid` is 0, and `private_directory_prepares_foreign_owned_staging_directory` is still ignored.
- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_macos.rs` (`clone_declines_inherited_read_denial_before_creation`, `clone_declines_before_creation_when_owner_read_is_absent`, `private_directory_prepares_foreign_owned_staging_directory`), `docs/filesystem-testing.md`
- **Problem:** The foreign-owner clone branches and staging case require native root access to change ownership and drop credentials. Noninteractive sudo still requires a password on this host, so ordinary-user passes do not establish those branches.
- **Recommendation:** Run the two clone-decline cases as root and the ignored foreign-owned staging case using the commands in the filesystem testing guide. Close this ticket only after those native branches pass. The implementation and support-boundary decisions are settled.
- **Fix with:** none
- **Effort:** S (hours)
