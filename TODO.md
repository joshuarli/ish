Prefix: ISH
Next: 62

## Accepted owner decisions — 2026-10-22

The decisions below are accepted. Their RFCs remain the implementation
contracts, with acceptance criteria. Retain accepted decisions after delivery
and update the shipped behavior references when implementation lands.

This table records accepted decisions, implementation dependencies, and
verification limits. Open implementation and acceptance work is listed below.

| Tickets | Accepted direction | Implementation dependency |
|---|---|---|
| [ISH-24](docs/rfc/ISH-24-builtin-allowlist.md) | Finite builtin policy; environment-only `set`; shared dispatch/lookup/completion rules | Implemented; focused Linux acceptance passed |
| [ISH-26](docs/rfc/ISH-26-denv-sourced-file-trust.md) | Retire implicit sourcing helpers; keep trusted `.env` assignment parsing | Implemented; both evaluators and rejection regressions passed |
| [ISH-15, ISH-52](docs/rfc/ISH-15-environment-and-shell-ownership.md) | Explicit environments; remove host environment mutation and unsupported safety comments | Implemented; focused Linux acceptance passed; macOS remains under ISH-45 |
| [ISH-5](docs/rfc/ISH-5-foreground-job-outcomes.md) | Structured stopped-job handles; epsh owns every child wait | Implemented; Linux PTY and process tests passed; macOS is tracked under ISH-45 |
| [ISH-8](docs/rfc/ISH-8-callback-process-boundary.md) | Creator-process callbacks by default; explicit unsafe child opt-in | Implemented after ISH-5; Linux callback boundary tests passed; macOS is tracked under ISH-45 |
| [ISH-32](docs/rfc/ISH-32-capture-and-retire.md) | Shared journaled fdu quarantine and identity primitives | Linux x86_64 and ARM64 musl acceptance passed, including Btrfs FICLONE; ARM64 ran in an OrbStack VM; macOS under ISH-45 |
| [ISH-42](docs/rfc/ISH-42-release-delivery.md) | Default-branch releases, verified bytes, diagnostic panic aborts | Linux implementation verified; native macOS release build and panic diagnostic passed |
| [ISH-48](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor) | Private binary `Shell`; reusable library editor | Implemented; focused Linux editor and PTY acceptance passed |

## Open implementation tickets

### ISH-45 · P3 · macOS restrictive-umask and privileged acceptance remain incomplete

- **Status:** Native ARM64 compilation, strict Clippy, 178 PTY tests, ACL and sparse transfer regressions, a complete resource-fork copy above 4 GiB, and release panic diagnostics passed. Full workspace and integrated test acceptance remain incomplete.
- **Area:** Tests
- **Where:** `crates/fdu-posix/src/quarantine.rs` (`open_created_directory`), `crates/fdu-posix/tests/recovery.rs`, `crates/ish-fs/src/platform/macos.rs` (`prepare_directory`), `crates/ish-fs/tests/native_macos.rs`, `crates/ish-fs/tests/transfer.rs`, `docs/filesystem-testing.md`
- **Problem:** A process umask of `0777` creates mode-000 staging directories. Darwin refuses the current `O_EVTONLY` open with EACCES, so the code cannot acquire a descriptor for safe permission normalization. The workspace run fails the restrictive-umask recovery, native directory, and transfer cases. Path-based chmod with before/after identity checks would introduce a substitution race; process-global umask changes or filesystem helper children would change existing boundaries. Privileged macOS ownership cases remain unrun because noninteractive sudo requires a password. `make test` also encountered one unexplained retained conformance mismatch, although focused and full conformance reruns passed.
- **Recommendation:** Resolve secure native creation/access under a restrictive umask, or obtain an owner decision about that support boundary; preserve the failing tests and fail-closed behavior meanwhile. Run the remaining foreign-owner cases with native root access. Complete ordinary-user workspace tests and `make test`, including explaining the intermittent conformance failure tracked under ISH-61. Reuse the native compilation, PTY, filesystem, release, and Apple utility evidence while its inputs remain unchanged; exact commands and limits are in the filesystem testing guide.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-60 · P3 · Journaled retirement makes large flat EXDEV moves take minutes

- **Status:** Candidate parked at `e2f0f08` on `backlog-retirement-2026-10-23`; it is not integrated. Focused correctness, mount fixtures, and complete baseline/candidate 4k and 16k output verification passed. Timing proof is deferred because unrelated host builds overlapped samples.
- **Area:** Performance
- **Where:** `crates/ish-fs/src/engine/retirement.rs`, `crates/fdu-posix/src/quarantine.rs`, `benches/fs/results-linux.md`
- **Problem:** On Linux x86_64 ext4-to-tmpfs, the 16,384-file, 64 MiB EXDEV engine sample at source revision `0494a1e8c457ee69a2281a9f2fbb02e85036d4e6` took 300.993 seconds wall and 199.162 seconds CPU; source retirement accounted for 264.073 seconds. Five 4,096-file samples had a 43.331 second median wall time and 13.009 second median CPU, with wide wall-time spread. ARM64 Linux syscall profiles now identify repeated destination-root recovery scans. The parked candidate holds recovery ownership for flat destination roots, preserves namespace rechecks and per-entry synchronization, and passes focused tests; source-root scans remain. No valid timing comparison establishes its benefit. Its profiles, resource observations, and discarded timing attempts are recorded in `benches/fs/results-linux.md` on the parked branch.
- **Recommendation:** On a quiet host, collect five interleaved baseline/candidate 4,096-file samples with the same durable journal guarantees, readiness checks before and immediately after each sample, and full output/source-retirement verification. Reuse the retained correctness and resource evidence where applicable, and qualify the 1k and 16k observations separately. Integrate the candidate only if its measured benefit exceeds observed spread without weakening intent durability, source-version validation, recovery behavior, or destination receipts. Current result: `perf gate deferred: host busy`.
- **Fix with:** make-it-fast
- **Effort:** L (weeks)

## Next

### ISH-61 · P3 · Native macOS conformance intermittently takes a subshell error branch

- **Area:** Tests
- **Where:** `crates/epsh/check-epsh.t:583` (`expand-unglob-unq`), `crates/epsh/tests/conformance/executor.rs:423`
- **Problem:** During ordinary-user `make test` on macOS 27.0.1 ARM64, the retained case exited 0 but printed `9 qstn brac -> error` where `9 qstn brac foo x c x} baz` was required. Every other output line matched. The expected-success subshell redirects stderr to `/dev/null`, so its failure reason was not retained. The targeted case and the complete 175-case corpus passed afterward (167 passed, 8 declared exclusions). The cause is unproven; reruns do not establish that the intermittent failure is fixed. epsh production evaluation code was unchanged in this backlog pass.
- **Recommendation:** Reproduce the retained case with `EPSH_CONFORMANCE_ID=expand-unglob-unq cargo test -p epsh --test conformance -- --nocapture`, including the original parallel-build conditions where useful. Observe the failing subshell's spawn, wait, and error path without changing the retained input or expected bytes. Fix only an established cause and retain the behavioral regression; keep the case required to pass.
- **Fix with:** root-cause
- **Effort:** M (days)
