Prefix: ISH
Next: 60

## Accepted owner decisions — 2026-10-22

The nine decisions below are accepted. Their implementation is still open;
this documentation update does not close defects or change runtime behavior.
The linked RFCs are the implementation contracts, with acceptance criteria.
Retain accepted decisions after implementation and update the shipped behavior
references when the code lands.

**Next implementation: ISH-24.** Close the interactive builtin boundary first,
including argument checks for `set` and nested `command` dispatch. ISH-26 and
ISH-42 can proceed independently. Implement ISH-15 and ISH-52 together; coordinate
ISH-5/8 on epsh dispatch and child ownership, and use the resulting boundaries
when extracting the editor for ISH-48. ISH-32 is a separate filesystem change
whose closure requires native Linux and macOS evidence.

| Tickets | Accepted direction | Implementation dependency |
|---|---|---|
| [ISH-24](docs/rfc/ISH-24-builtin-allowlist.md) | Finite builtin policy; environment-only `set`; shared dispatch/lookup/completion rules | Ready; coordinate argv policy with ISH-8 |
| [ISH-26](docs/rfc/ISH-26-denv-sourced-file-trust.md) | Retire implicit sourcing helpers; keep trusted `.env` assignment parsing | Ready; preserve semantics through ISH-15 |
| [ISH-15, ISH-52](docs/rfc/ISH-15-environment-and-shell-ownership.md) | Explicit environments; remove host environment mutation and unsupported safety comments | One implementation and closure |
| [ISH-5](docs/rfc/ISH-5-foreground-job-outcomes.md) | Structured stopped-job handles; epsh owns every child wait | Ready; coordinate with ISH-8 |
| [ISH-8](docs/rfc/ISH-8-callback-process-boundary.md) | Creator-process callbacks by default; explicit unsafe child opt-in | Coordinate external dispatch with ISH-5 and policy with ISH-24 |
| [ISH-32](docs/rfc/ISH-32-capture-and-retire.md) | Shared journaled fdu quarantine and identity primitives | Native recovery validation; account for ISH-45 |
| [ISH-42](docs/rfc/ISH-42-release-delivery.md) | Default-branch releases, verified bytes, diagnostic panic aborts | Ready; independent |
| [ISH-48](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor) | Private binary `Shell`; reusable library editor | Extraction can proceed; final integration follows ISH-15's environment contract |

## Open implementation tickets

### ISH-26 · P2 · A trusted `.envrc` runs unhashed files through `source_env`, `source_up` and `dotenv`

- **Status:** Decision accepted; implementation pending.
- **Area:** Security
- **Where:** `src/denv_stdlib.sh:153-189`, `src/denv.rs:1071-1079`, `src/denv.rs:466-475`
- **Problem:** The stdlib is prepended to every `.envrc` evaluation. `source_env`, `source_env_if_exists`, `source_up`, `dotenv` and `dotenv_if_exists` run `. "$file"`. Those files never reach `read_trusted_candidate` or `is_allowed_in`. A trusted child `.envrc` that calls `source_up` executes the nearest parent `.envrc` even if it is untrusted, edited after `denv allow`, or revoked with `denv deny`. A trusted `source_env ./shared.sh` runs whatever a later `git pull` put there, and `dotenv` executes `.env` as shell code. README says editing a trusted file revokes trust, untrusted discovered files are never loaded, and `.env` is read as variable assignments. Read from code; no end-to-end run.
- **Recommendation:** Implement the accepted retirement of all six implicit sourcing helpers, with diagnostic-only refusals that invalidate the candidate even when a script masks the immediate status. Keep automatic trusted `.env` parsing, preserve `.env` precedence, and document that approved `.envrc` code is not sandboxed. Add both-interpreter rejection and trust-regression coverage.
- **Fix with:** os-boundaries
- **Effort:** M (days)
- **Design:** [Trust for sourced denv files](docs/rfc/ISH-26-denv-sourced-file-trust.md)

### ISH-24 · P2 · plan() admits epsh builtins the contract never lists; `exec` ends the shell and `trap` disables signal handling

- **Status:** Decision accepted; implementation pending.
- **Area:** Security
- **Where:** `src/dispatch.rs:79-93`, `src/dispatch.rs:142-151`, `crates/epsh/src/builtins.rs:165-177`, `crates/epsh/src/builtins.rs:617-654`, `crates/epsh/src/builtins.rs:783-838`, `crates/epsh/src/signal.rs:77-95`, `src/signal.rs:29-37`
- **Problem:** `plan()` and the dynamic target check refuse only `eval`, `source`, `.` and ish shell-state builtins. Every other name in epsh's `BUILTIN_NAMES` runs inside ish. Reproduced in a PTY on the debug build: `exec echo replaced` prints `replaced` and the session ends without restoring raw mode or flushing state. Per the epsh lane's PTY run, `trap x TERM` replaces ish's SIGTERM handler with one that sets a flag nothing reads, so SIGTERM no longer stops ish; trap actions never run because ish does not call `run_pending_traps`. `set -e`, `set -o noexec`, `wait`, `local`, `umask`, `getopts` and `read` also pass; those were not exercised. README and AGENTS.md list none of them.
- **Recommendation:** Implement the accepted finite policy in `builtin.rs`, shared by planning, expanded argv checks, completion, and lookup. Restrict `set` to listing and named-variable assignment; validate nested `command` before target redirections. Preserve approved interactive commands and standalone epsh/denv behavior. Replace the permissive `set` option PTY expectation.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** M (days)
- **Design:** [Builtin allowlist](docs/rfc/ISH-24-builtin-allowlist.md)

### ISH-5 · P2 · Stopped pipelines disappear from foreground job control

- **Status:** Decision accepted; implementation pending.
- **Area:** Concurrency
- **Where:** `src/main.rs`, `src/external.rs`, `crates/epsh/src/eval.rs`, `src/job.rs`
- **Problem:** epsh reports a stopped pipeline with PID and PGID, then `run_program` discards those IDs and returns 148. ish records a suspended job only through its external-handler `STOPPED_JOB` slot. The earlier PTY reproduction returned the prompt after stopping a pipeline, but `fg` reported no suspended job while children remained stopped. Resume still waits for only one group member. Current `job::resume_job` retains its slot on non-ECHILD wait errors; the earlier unconditional-drop claim is stale. Numeric status, a thread-local stop, and a slot flag still obscure ownership and can confuse a real exit 148 with a stop.
- **Recommendation:** Implement the accepted structured execution outcome and opaque job handle. epsh owns all tracked child waits and cleanup; ish owns its one suspended slot and terminal/capture UI. Delegate normal external spawning to epsh, remove numeric/TLS stop transport, and test all pipeline members through repeated stop/resume and errors.
- **Fix with:** lifecycle-concurrency, error-model
- **Effort:** M (days)
- **Design:** [Foreground job outcomes](docs/rfc/ISH-5-foreground-job-outcomes.md)

### ISH-8 · P2 · Forked external callbacks can deadlock on captured host locks

- **Status:** Decision accepted; implementation pending.
- **Area:** Concurrency
- **Where:** `crates/epsh/src/eval.rs`
- **Problem:** ExternalHandler is Send but can capture a mutex held by another host thread when command substitution forks. The child invokes the copied callback with a permanently locked copy. A fresh-source library probe completed with status 0 when unlocked; when held across fork, the parent thread released at 99 ms but the child stayed blocked until the one-second shell timeout returned 130.
- **Recommendation:** Implement creator-process-only callbacks through safe APIs, with typed child refusal and separate unsafe registration for audited child execution. Apply the rule to external, policy/argv-policy, and redirect callbacks; propagate substitution refusals without target effects. Preserve ish pipelines using owned policy data and individually justified internal boundaries; no broker or silent fallback.
- **Fix with:** lifecycle-concurrency, api-surface-and-module-boundaries
- **Effort:** L (weeks)
- **Design:** [Callback process boundary](docs/rfc/ISH-8-callback-process-boundary.md)

### ISH-15 · P2 · Safe library APIs conceal environment mutation preconditions

- **Status:** Decision accepted; implementation pending.
- **Area:** Security
- **Where:** `src/lib.rs`, `src/denv.rs`
- **Problem:** Public safe wrappers call unsafe std environment mutation without enforcing its Unix process-wide concurrency preconditions. Public denv operations also reach those mutations. Safe library callers can invoke them while another thread uses libc environment readers. The pinned standard-library safety contract explicitly names such readers, including DNS resolution. This is a source-proven missing safety boundary; a concrete undefined-behavior manifestation is unproven and was not attempted. Normal ish foreground execution and joined relay threads do not establish an observed incident.
- **Recommendation:** Remove all production process-environment mutation, including the public wrappers, denv temporary environment swaps, and binary startup/PWD mirrors. Use epsh-owned values, explicit byte-preserving evaluator/child environments, and a validated denv delta. Audit ambient readers and restoration semantics; implement ISH-52 in the same change.
- **Fix with:** os-boundaries, api-surface-and-module-boundaries
- **Effort:** M (days)
- **Design:** [Environment and shell ownership](docs/rfc/ISH-15-environment-and-shell-ownership.md)

### ISH-32 · P3 · Two capture-and-retire protocols and duplicated identity code across `fdu-*` and `ish-fs`

- **Status:** Decision accepted; implementation pending.
- **Area:** Architecture
- **Where:** `crates/fdu-delete/src/posix/quarantine.rs`, `crates/ish-fs/src/engine/retirement.rs:58-100`, `crates/ish-fs/src/engine/mod.rs:31`, `crates/fdu-delete/src/lib.rs:783-834`, `crates/fdu-scan/src/fsutil.rs:20`, `crates/ish-fs/src/roots.rs:18`, `docs/filesystem.md:210-217`
- **Problem:** Both crates implement rename to a hidden name, verify identity, unlink, restore on mismatch. Only `fdu-delete` is journaled and recovers on the next run; ish-fs leaves `.ish-fs-retired-*`, `.ish-fs-intent-*` and `.ish-fs-stage-*` names after SIGKILL or power loss with no sweep, and names built from pid plus a counter can collide with an orphan under pid reuse. `FileIdentity` and `Identity`, four mount-identity helpers, three EINTR retry loops and two macOS `fsid_t` `transmute_copy` calls exist in parallel, each with its own `cfg` and fixes.
- **Recommendation:** Extract the accepted journaled fdu quarantine and shared identity/mount primitives into `fdu-posix`, then migrate ish-fs without weakening source-version or durable-destination checks. Recovery restores names without overwrite and requires a fresh plan/retry; preserve and report ambiguous, staged, and legacy objects. Close only after both callers use the protocol and native recovery tests pass.
- **Fix with:** single-source-of-truth, durable-state
- **Effort:** L (weeks)
- **Design:** [Shared capture and retirement](docs/rfc/ISH-32-capture-and-retire.md)

### ISH-42 · P3 · Release delivery lacks a ref guard and integrity checks, and release panics are silent

- **Status:** Decision accepted; implementation pending.
- **Area:** Build
- **Where:** `.github/workflows/release.yml:4`, `:22`, `:110-116`, `scripts/check-refinement.py:26-29`, `:48-52`, `:89-93`, `Makefile:11`, `Dockerfile:124`, `Cargo.toml:51`
- **Problem:** `workflow_dispatch` publishes a pre-release from any branch with a zip only, no checksum, and the build job holds `actions: write` with no visible use. Rollback is deleting `release-<sha>` by hand. `check-refinement.py` runs an Aeneas tarball and `rustup-init` downloaded without digests, while the Dockerfile verifies equivalents with `sha256sum`. Release uses `-Cpanic=immediate-abort`, which skips the panic hook, so a panic ends in SIGABRT with no message; not run, inferred from the flag.
- **Recommendation:** Implement the accepted release policy: fail non-default-branch dispatches, bind publication to the dispatch SHA, validate the expected artifacts and publish final-ZIP SHA256SUMS, remove unused permissions, verify pinned tool downloads/caches, and use diagnostic-preserving `panic=abort` instead of immediate-abort.
- **Fix with:** none
- **Effort:** S (hours)
- **Design:** [Release delivery](docs/rfc/ISH-42-release-delivery.md)

### ISH-45 · P3 · macOS-only edits from the backlog run were never compiled

- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_macos.rs`, `crates/ish-fs/src/engine/mod.rs` (`replace_by_exchange`), `crates/fdu-scan/src/macos.rs`, `src/signal.rs` (`errno_slot`), `crates/ish-fs/src/platform/mod.rs` (`rename_exchange`), `crates/fdu-scan/src/fsutil.rs` (`identity_link_count_and_modified_time` is now `cfg(target_os = "macos")`)
- **Problem:** ISH-29, ISH-31, ISH-33 and ISH-38 changed macOS code paths on a Linux host. `native_macos.rs` now panics instead of skipping, `rename_exchange` relies on rustix mapping `RenameFlags::EXCHANGE` to `RENAME_SWAP`, `fdu-scan/src/macos.rs` lost its one-shot scanner, and the signal handler uses `libc::__error`. None of it was compiled or run.
- **Recommendation:** On a macOS host run workspace check, strict clippy, `make test`, and `cargo test -p ish-fs --test native_macos -- --include-ignored` as uid 0, and fix what fails.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-48 · P3 · `main.rs` still holds the editor and `Shell`, and the keystroke path has no unit seam

- **Status:** Decision accepted; implementation pending.
- **Area:** Architecture
- **Where:** `src/main.rs`, `docs/architecture.md`
- **Problem:** ISH-34 moved the external handler, diagnostics, `cd` rewriting and mode state out and split `read_line_with_reader` per event and mode, but `main.rs` still holds the interactive `Shell` and editor. Per-mode handlers take runtime state and terminal-facing collaborators instead of a narrow event-transition seam. Binary modules can have unit tests; the missing separation of state from effects is the testing obstacle. `write_layout_dump_file` also retries every `open` error and eventually reports "could not choose a unique dump filename", hiding non-collision failures.
- **Recommendation:** Keep interactive `Shell` private in binary module `src/shell.rs`; extract reusable editor state and mode/key handlers into library `src/editor.rs` with narrow context and typed actions. Keep process/terminal/job ownership in the binary, preserve buffer reuse, add handler unit coverage, and return the dump writer's real non-collision open errors.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** L (weeks)
- **Design:** [Private session, testable editor](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-48-private-session-testable-editor)

### ISH-52 · P4 · Two SAFETY claims in `denv.rs` say ish is single-threaded without proof

- **Status:** Decision accepted; blocked on the ISH-15 implementation and closed with it.
- **Area:** Security
- **Where:** `src/denv.rs` (`env::set_var` and `env::remove_var` call sites)
- **Problem:** The comments claim a single-threaded process, but `foreground.rs` spawns worker threads. This overlaps ISH-15 and was left unverified by the comment cleanup.
- **Recommendation:** Implement and close with ISH-15 by deleting environment mutation and the unsupported single-threaded claims, then audit remaining unsafe boundaries for their actual preconditions. Do not replace the comments with an unproven worker-lifetime assertion.
- **Fix with:** os-boundaries
- **Effort:** S (hours)
- **Design:** [Remove the unsupported safety claims](docs/rfc/ISH-15-environment-and-shell-ownership.md#ish-52-remove-the-unsupported-safety-claims)

### ISH-59 · P4 · The prompt's `NoRepo` cache path has no test

- **Area:** Tests
- **Where:** `src/prompt.rs` (`GitCache::NoRepo`, `resolve_git`), `tests/integration.rs`
- **Problem:** ISH-57 deleted `prompt_git_cache_no_repo_reuse`, which only asserted `ends_with(" $ ")`. A meaningful version needs a directory with no repository above it, and the test scratch directories sit under the checkout's `.git`, so nothing exercises the `NoRepo` cache reuse and invalidation.
- **Recommendation:** Add a unit test in `src/prompt.rs` that builds the cache state directly (or a scratch root outside any repository) and checks that a second render in the same directory skips the ancestor walk and that `invalidate_git` clears it.
- **Fix with:** test-audit
- **Effort:** S (hours)
