Prefix: ISH
Next: 61

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
| [ISH-32](docs/rfc/ISH-32-capture-and-retire.md) | Shared journaled fdu quarantine and identity primitives | Linux x86_64 acceptance passed; successful clone and native ARM64 evidence remain pending; macOS under ISH-45 |
| [ISH-42](docs/rfc/ISH-42-release-delivery.md) | Default-branch releases, verified bytes, diagnostic panic aborts | Linux implementation verified; native macOS panic probe is tracked under ISH-45 |
| [ISH-48](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor) | Private binary `Shell`; reusable library editor | Implemented; focused Linux editor and PTY acceptance passed |

## Open implementation tickets

### ISH-32 · P3 · Native clone and ARM64 acceptance for shared quarantine remains incomplete

- **Status:** Linux x86_64 implementation and owned acceptance fixtures passed; required native evidence remains incomplete.
- **Area:** Tests
- **Where:** `crates/fdu-posix`, `crates/fdu-delete`, `crates/fdu-scan`, `crates/ish-fs`, `docs/filesystem-testing.md`
- **Problem:** Both callers now use shared journaled quarantine and identity primitives. Linux x86_64 mount recovery, foreign-owner behavior, and deferred blocking flags passed. The required FICLONE case returned `EOPNOTSUPP` on ext4, and native ARM64 musl hardware is unavailable here. Native macOS acceptance remains tracked under ISH-45.
- **Recommendation:** Run the named FICLONE case on an owned clone-capable filesystem and complete workspace, musl, mount, and filesystem acceptance on native ARM64. Keep the ticket open until these required cases execute and pass; use the evidence ledger for exact commands and limits.
- **Fix with:** none
- **Effort:** S (hours)
- **Design:** [Shared capture and retirement](docs/rfc/ISH-32-capture-and-retire.md)

### ISH-45 · P3 · macOS platform build and native acceptance remain unverified

- **Area:** Tests
- **Where:** `src/capture.rs:202,227`, `src/denv.rs:96`, `src/diagnostics.rs:175`, `src/prompt.rs:475`, `crates/ish-fs/tests/native_macos.rs`, `crates/ish-fs/src/engine/mod.rs` (`replace_by_exchange`), `crates/fdu-scan/src/macos.rs`, `src/signal.rs` (`errno_slot`), `crates/ish-fs/src/platform/mod.rs` (`rename_exchange`), `crates/fdu-scan/src/fsutil.rs` (`identity_link_count_and_modified_time` is now `cfg(target_os = "macos")`)
- **Problem:** ISH-29, ISH-31, ISH-33 and ISH-38 changed macOS code paths on a Linux host. The latest macOS CI run fails to compile because the pinned rustix lacks `OpenptFlags::CLOEXEC`, `pipe_with`/`PipeFlags`, `rand::getrandom`/`GetRandomFlags`, and `fs::mknodat` at the listed call sites. `native_macos.rs` also changes a caller-owned destination to mode `0044` and then reopens its path, which fails for an unprivileged owner. Existing paths still need compilation and native validation.
- **Recommendation:** On a native macOS host, first run workspace check, strict all-target Clippy, workspace tests, and `make test` as an ordinary user; repair unsupported calls with platform-supported APIs while preserving descriptor ownership, close-on-exec, cleanup, secure randomness, and FIFO setup. Fix the read-only destination test using a descriptor opened before chmod or restore access only for fixture verification. Run native clone/access normalization, ACL, foreign-owner, blocking-flag, cancellation, sparse fallback, and resource-fork cases, with privileged cases separately. Compare clone-capable Apple `cp`/`cp -c` and GNU `cp --reflink=auto` on supported native filesystems, recording selected methods and preservation/durability differences. Build a panic probe with the actual release flags and verify its diagnostic on macOS; record binary size as informational evidence.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-60 · P3 · Journaled retirement makes large flat EXDEV moves take minutes

- **Area:** Performance
- **Where:** `crates/ish-fs/src/engine/retirement.rs`, `crates/fdu-posix/src/quarantine.rs`, `benches/fs/results-linux.md`
- **Problem:** On Linux x86_64 ext4-to-tmpfs, the 16,384-file, 64 MiB EXDEV engine sample at source revision `0494a1e8c457ee69a2281a9f2fbb02e85036d4e6` took 300.993 seconds wall and 199.162 seconds CPU; source retirement accounted for 264.073 seconds. Five 4,096-file samples had a 43.331 second median wall time and 13.009 second median CPU, with wide wall-time spread. The output and every named source retirement were verified. The retirement phase is the observed cost center; a syscall profile and matched baseline using the durable journal are absent.
- **Recommendation:** Profile the per-entry retirement path and reduce its large-tree cost without weakening intent durability, source-version validation, recovery behavior, or durable destination checks. Re-run the same readiness-gated workloads and compare against a matched implementation with the same guarantees.
- **Fix with:** make-it-fast
- **Effort:** L (weeks)
