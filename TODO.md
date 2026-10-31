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

- **Status:** Handoff. The history rewrite landed on `master` in `c917084`, `29481bc`, `5fc37a3`, `e88628c`, `c74647f`, `8361bcb`. Two ranked leads remain unprofiled, one is deferred by the owner, and one approach is closed. No work is in flight; the tree is clean.
- **Area:** Performance
- **Where:** `benches/bench.rs`, `src/history.rs`, `src/complete.rs`, `src/ls.rs`, `src/shell.rs`, `docs/verification/history-search-profile.md`
- **Problem:** `History` is 5.81 MB at 45k unique commands, down from 22.32 MB (129 B per entry, command text is 1.53 MB of it). Every remaining benchmark still sits where it did before that work. Last known state, taken while the host was busy, so treat the times as a starting point and re-baseline: `history_search_trace` 2.0–2.5 ms, `history_fuzzy_search_in_dir_45k` 584–654 µs, `history_fuzzy_search_into_45k` 588–735 µs, `startup_warm` 550–690 µs, `startup_cold` 8.7–9.3 ms, `completion_path_realistic` 250–400 µs, `ls_fixture_dir` 176–193 µs, `cd_prompt_with_denv` 490–780 µs against `cd_prompt_without_denv` 55–86 µs, `command_enter_to_prompt` 65–96 µs, `autosuggestion_prefix_search_miss` 90–150 µs, keypress render 42–83 ns.
- **Recommendation:** Work the leads in order, one attributable change at a time, keeping it only when the paired ratio beats the observed spread.
  1. `history_search_trace` is now the largest history cost and has never been profiled. It runs an empty query, so matching is skipped and the cost is the 45k candidate scan plus `HistoryPagerCache` and `RenderedRegion`. Profile before changing anything.
  2. `history_fuzzy_search_into_45k` runs query `gco` against entries with no directory data, so the ranking is cheap and the cost is the linear text scan. Isolating classification on the in-directory fixture with a zero limit put it at ~430 µs for a one-character query; re-isolate for `gco` before choosing between a first-byte index and a cheaper matcher.
  3. `startup_cold` is dominated by the legacy migration path, which re-parses the text history and writes 2000 framed records per run. It is paid once per upgrade, so weigh any change against that.
  4. `cd_prompt_with_denv` costs roughly 8x the plain path. The owner deferred this; do not take it without asking.
  5. Not worth redoing: `bbtree` (`docs/decisions.md`), a top-K `BinaryHeap` for history ranking (no measurable difference), and skipping the denv variable clone (no measurable difference).
  6. Closed: prefix-compressing the command arena. `History::get` returns `&str` borrowed from the arena and the search reads it per candidate per keystroke, so front-coding would force an allocation or `Cow` on the hottest path.
- **Measurement:** `perf-gate-ready` can exit 0 while another workload saturates the host, so check `uptime` and `ps -Ao pcpu,comm -r` too. Work in two worktrees and compare with `rustybench baseline --root <wt> --baseline <json> -- cargo bench --bench bench` followed by `rustybench diff`, interleaving baseline and candidate. Repeated runs of the same binary spread 20–40 percent on a busy host, so trust paired ratios, not single runs. The allocator counters in the bench report are deterministic and stay usable when timing does not. For attribution, run the workload in a loop long enough to sample and use `sample <pid> 4 -file <out>`; its leaf-stack table named the directory hash that the history fix removed. `xctrace export` did not carry backtraces. Scratch probes used this session lived in `.work/tmp` and are disposable; rebuild them from the benchmark fixtures in `benches/bench.rs` rather than relying on them.
- **Fix with:** make-it-fast
- **Effort:** M

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
