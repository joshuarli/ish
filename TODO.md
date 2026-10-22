Prefix: ISH
Next: 60

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
| [ISH-32](docs/rfc/ISH-32-capture-and-retire.md) | Shared journaled fdu quarantine and identity primitives | Native recovery validation; account for ISH-45 |
| [ISH-42](docs/rfc/ISH-42-release-delivery.md) | Default-branch releases, verified bytes, diagnostic panic aborts | Linux implementation verified; native macOS panic probe is tracked under ISH-45 |
| [ISH-48](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor) | Private binary `Shell`; reusable library editor | Implemented; focused Linux editor and PTY acceptance passed |

## Open implementation tickets

### ISH-32 · P3 · Two capture-and-retire protocols and duplicated identity code across `fdu-*` and `ish-fs`

- **Status:** Decision accepted; implementation pending.
- **Area:** Architecture
- **Where:** `crates/fdu-delete/src/posix/quarantine.rs`, `crates/ish-fs/src/engine/retirement.rs:58-100`, `crates/ish-fs/src/engine/mod.rs:31`, `crates/fdu-delete/src/lib.rs:783-834`, `crates/fdu-scan/src/fsutil.rs:20`, `crates/ish-fs/src/roots.rs:18`, `docs/filesystem.md:210-217`
- **Problem:** Both crates implement rename to a hidden name, verify identity, unlink, restore on mismatch. Only `fdu-delete` is journaled and recovers on the next run; ish-fs leaves `.ish-fs-retired-*`, `.ish-fs-intent-*` and `.ish-fs-stage-*` names after SIGKILL or power loss with no sweep, and names built from pid plus a counter can collide with an orphan under pid reuse. `FileIdentity` and `Identity`, four mount-identity helpers, three EINTR retry loops and two macOS `fsid_t` `transmute_copy` calls exist in parallel, each with its own `cfg` and fixes.
- **Recommendation:** Extract the accepted journaled fdu quarantine and shared identity/mount primitives into `fdu-posix`, then migrate ish-fs without weakening source-version or durable-destination checks. Recovery restores names without overwrite and requires a fresh plan/retry; preserve and report ambiguous, staged, and legacy objects. Close only after both callers use the protocol and native recovery tests pass.
- **Fix with:** single-source-of-truth, durable-state
- **Effort:** L (weeks)
- **Design:** [Shared capture and retirement](docs/rfc/ISH-32-capture-and-retire.md)

### ISH-45 · P3 · macOS platform build and native acceptance remain unverified

- **Area:** Tests
- **Where:** `src/capture.rs:202,227`, `src/denv.rs:96`, `src/diagnostics.rs:175`, `src/prompt.rs:475`, `crates/ish-fs/tests/native_macos.rs`, `crates/ish-fs/src/engine/mod.rs` (`replace_by_exchange`), `crates/fdu-scan/src/macos.rs`, `src/signal.rs` (`errno_slot`), `crates/ish-fs/src/platform/mod.rs` (`rename_exchange`), `crates/fdu-scan/src/fsutil.rs` (`identity_link_count_and_modified_time` is now `cfg(target_os = "macos")`)
- **Problem:** ISH-29, ISH-31, ISH-33 and ISH-38 changed macOS code paths on a Linux host. The latest macOS CI run fails to compile because the pinned rustix lacks `OpenptFlags::CLOEXEC`, `pipe_with`/`PipeFlags`, `rand::getrandom`/`GetRandomFlags`, and `fs::mknodat` at the listed call sites. `native_macos.rs` also changes a caller-owned destination to mode `0044` and then reopens its path, which fails for an unprivileged owner. Existing paths still need compilation and native validation.
- **Recommendation:** On a native macOS host, first run workspace check, strict all-target Clippy, workspace tests, and `make test` as an ordinary user; repair unsupported calls with platform-supported APIs while preserving descriptor ownership, close-on-exec, cleanup, secure randomness, and FIFO setup. Fix the read-only destination test using a descriptor opened before chmod or restore access only for fixture verification. Run native clone/access normalization, ACL, foreign-owner, blocking-flag, cancellation, sparse fallback, and resource-fork cases, with privileged cases separately. Compare clone-capable Apple `cp`/`cp -c` and GNU `cp --reflink=auto` on supported native filesystems, recording selected methods and preservation/durability differences. Build a panic probe with the actual release flags and verify its diagnostic on macOS; record binary size as informational evidence.
- **Fix with:** none
- **Effort:** S (hours)
