Prefix: ISH
Next: 53

### ISH-26 · P2 · A trusted `.envrc` runs unhashed files through `source_env`, `source_up` and `dotenv`

- **Area:** Security
- **Where:** `src/denv_stdlib.sh:153-189`, `src/denv.rs:1071-1079`, `src/denv.rs:466-475`
- **Problem:** The stdlib is prepended to every `.envrc` evaluation. `source_env`, `source_env_if_exists`, `source_up`, `dotenv` and `dotenv_if_exists` run `. "$file"`. Those files never reach `read_trusted_candidate` or `is_allowed_in`. A trusted child `.envrc` that calls `source_up` executes the nearest parent `.envrc` even if it is untrusted, edited after `denv allow`, or revoked with `denv deny`. A trusted `source_env ./shared.sh` runs whatever a later `git pull` put there, and `dotenv` executes `.env` as shell code. README says editing a trusted file revokes trust, untrusted discovered files are never loaded, and `.env` is read as variable assignments. Read from code; no end-to-end run.
- **Recommendation:** Decide the trust model for sourced files, then enforce it: remove these functions, or route them through an ish-provided check that hashes the target and consults the allow store. Make `dotenv` parse assignments instead of sourcing.
- **Fix with:** os-boundaries
- **Effort:** M (days)
- **Design:** [Trust for sourced denv files](docs/rfc/ISH-26-denv-sourced-file-trust.md)

### ISH-24 · P2 · plan() admits epsh builtins the contract never lists; `exec` ends the shell and `trap` disables signal handling

- **Area:** Security
- **Where:** `src/dispatch.rs:79-93`, `src/dispatch.rs:142-151`, `crates/epsh/src/builtins.rs:165-177`, `crates/epsh/src/builtins.rs:617-654`, `crates/epsh/src/builtins.rs:783-838`, `crates/epsh/src/signal.rs:77-95`, `src/signal.rs:29-37`
- **Problem:** `plan()` and the dynamic target check refuse only `eval`, `source`, `.` and ish shell-state builtins. Every other name in epsh's `BUILTIN_NAMES` runs inside ish. Reproduced in a PTY on the debug build: `exec echo replaced` prints `replaced` and the session ends without restoring raw mode or flushing state. Per the epsh lane's PTY run, `trap x TERM` replaces ish's SIGTERM handler with one that sets a flag nothing reads, so SIGTERM no longer stops ish; trap actions never run because ish does not call `run_pending_traps`. `set -e`, `set -o noexec`, `wait`, `local`, `umask`, `getopts` and `read` also pass; those were not exercised. README and AGENTS.md list none of them.
- **Recommendation:** Replace the deny-list with an explicit allowlist of epsh builtins that ish exposes, shared by `plan()`, the dynamic check and completion, and refuse the rest with status 2 before evaluation. Add `dispatch::plan` tests for `exec` and `trap`. Document the allowlist in README.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** M (days)
- **Design:** [Builtin allowlist](docs/rfc/ISH-24-builtin-allowlist.md)

### ISH-5 · P2 · Stopped pipelines disappear from foreground job control

- **Area:** Concurrency
- **Where:** `src/main.rs`, `crates/epsh/src/eval.rs`, `src/job.rs`
- **Problem:** epsh reports a stopped pipeline with PID and PGID, then run_program discards those IDs and returns 148. ish records a suspended job only from its parent-process STOPPED_JOB slot. In a fresh-build PTY, stopping a pipeline returned the prompt, but `fg` reported no suspended job while the children remained stopped. The current resume path also waits for only one group member, and `job::resume_job` drops the job from its slot on any wait error. The stop is carried by a thread-local, the status 148 and a second slot flag (`src/main.rs:20-27`, `:337`, `:2791-2816`), so a stale thread-local entry can be adopted by a later command that exits 148.
- **Recommendation:** Preserve the structured stop outcome through the embedding boundary and define ownership of every pipeline member during stop, resume, cancellation, and reaping.
- **Fix with:** lifecycle-concurrency, error-model
- **Effort:** M (days)
- **Design:** [Foreground job outcomes](docs/rfc/ISH-5-foreground-job-outcomes.md)

### ISH-8 · P2 · Forked external callbacks can deadlock on captured host locks

- **Area:** Concurrency
- **Where:** `crates/epsh/src/eval.rs`
- **Problem:** ExternalHandler is Send but can capture a mutex held by another host thread when command substitution forks. The child invokes the copied callback with a permanently locked copy. A fresh-source library probe completed with status 0 when unlocked; when held across fork, the parent thread released at 99 ms but the child stayed blocked until the one-second shell timeout returned 130.
- **Recommendation:** Define and enforce the callback execution boundary for forked evaluators. Audit command-policy and redirect-open callbacks under the same boundary rather than treating Send as a fork-safety guarantee.
- **Fix with:** lifecycle-concurrency, api-surface-and-module-boundaries
- **Effort:** L (weeks)
- **Design:** [Callback process boundary](docs/rfc/ISH-8-callback-process-boundary.md)

### ISH-15 · P2 · Safe library APIs conceal environment mutation preconditions

- **Area:** Security
- **Where:** `src/lib.rs`, `src/denv.rs`
- **Problem:** Public safe wrappers call unsafe std environment mutation without enforcing its Unix process-wide concurrency preconditions. Public denv operations also reach those mutations. Safe library callers can invoke them while another thread uses libc environment readers. The pinned standard-library safety contract explicitly names such readers, including DNS resolution. This is a source-proven missing safety boundary; a concrete undefined-behavior manifestation is unproven and was not attempted. Normal ish foreground execution and joined relay threads do not establish an observed incident.
- **Recommendation:** Expose the required safety contract through every public mutation entry point, or remove process-environment mutation from the library interface in favor of explicit environments. Changing only the three wrappers leaves the denv boundary unresolved.
- **Fix with:** os-boundaries, api-surface-and-module-boundaries
- **Effort:** M (days)

### ISH-32 · P3 · Two capture-and-retire protocols and duplicated identity code across `fdu-*` and `ish-fs`

- **Area:** Architecture
- **Where:** `crates/fdu-delete/src/posix/quarantine.rs`, `crates/ish-fs/src/engine/retirement.rs:58-100`, `crates/ish-fs/src/engine/mod.rs:31`, `crates/fdu-delete/src/lib.rs:783-834`, `crates/fdu-scan/src/fsutil.rs:20`, `crates/ish-fs/src/roots.rs:18`, `docs/filesystem.md:210-217`
- **Problem:** Both crates implement rename to a hidden name, verify identity, unlink, restore on mismatch. Only `fdu-delete` is journaled and recovers on the next run; ish-fs leaves `.ish-fs-retired-*`, `.ish-fs-intent-*` and `.ish-fs-stage-*` names after SIGKILL or power loss with no sweep, and names built from pid plus a counter can collide with an orphan under pid reuse. `FileIdentity` and `Identity`, four mount-identity helpers, three EINTR retry loops and two macOS `fsid_t` `transmute_copy` calls exist in parallel, each with its own `cfg` and fixes.
- **Recommendation:** Choose one capture protocol and one identity and mount module. Until then, have ish-fs report orphan `.ish-fs-*` entries at operation start and use random names.
- **Fix with:** single-source-of-truth, durable-state
- **Effort:** L (weeks)

### ISH-42 · P3 · Release delivery lacks a ref guard and integrity checks, and release panics are silent

- **Area:** Build
- **Where:** `.github/workflows/release.yml:4`, `:22`, `:110-116`, `scripts/check-refinement.py:26-29`, `:48-52`, `:89-93`, `Makefile:11`, `Dockerfile:124`, `Cargo.toml:51`
- **Problem:** `workflow_dispatch` publishes a pre-release from any branch with a zip only, no checksum, and the build job holds `actions: write` with no visible use. Rollback is deleting `release-<sha>` by hand. `check-refinement.py` runs an Aeneas tarball and `rustup-init` downloaded without digests, while the Dockerfile verifies equivalents with `sha256sum`. Release uses `-Cpanic=immediate-abort`, which skips the panic hook, so a panic ends in SIGABRT with no message; not run, inferred from the flag.
- **Recommendation:** Decide whether dispatch is limited to the default branch and whether artifacts need checksums, drop unneeded `actions: write`, pin digests in the refinement script, and decide whether the size gain justifies silent aborts, documenting the outcome.
- **Fix with:** none
- **Effort:** S (hours)


### ISH-45 · P3 · macOS-only edits from the backlog run were never compiled

- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_macos.rs`, `crates/ish-fs/src/engine/mod.rs` (`replace_by_exchange`), `crates/fdu-scan/src/macos.rs`, `src/signal.rs` (`errno_slot`), `crates/ish-fs/src/platform/mod.rs` (`rename_exchange`)
- **Problem:** ISH-29, ISH-31, ISH-33 and ISH-38 changed macOS code paths on a Linux host. `native_macos.rs` now panics instead of skipping, `rename_exchange` relies on rustix mapping `RenameFlags::EXCHANGE` to `RENAME_SWAP`, `fdu-scan/src/macos.rs` lost its one-shot scanner, and the signal handler uses `libc::__error`. None of it was compiled or run.
- **Recommendation:** On a macOS host run workspace check, strict clippy, `make test`, and `cargo test -p ish-fs --test native_macos -- --include-ignored` as uid 0, and fix what fails.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-48 · P3 · `main.rs` still holds the editor and `Shell`, and the keystroke path has no unit seam

- **Area:** Architecture
- **Where:** `src/main.rs`, `docs/architecture.md`
- **Problem:** ISH-34 moved the external handler, diagnostics, `cd` rewriting and mode state out and split `read_line_with_reader` per event and mode, but `main.rs` is still about 2500 lines. The editor functions, key handlers and `Shell` live in the binary, so the per-mode handlers cannot be unit-tested; `write_layout_dump_file` also reports every `open` error except retry exhaustion as "could not choose a unique dump filename".
- **Recommendation:** Decide where `Shell` lives, move the editor and key handlers behind it into the library, and add unit tests for the per-mode handlers. Report the real `open` error from the dump writer.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** L (weeks)

### ISH-52 · P4 · Two SAFETY claims in `denv.rs` say ish is single-threaded without proof

- **Area:** Security
- **Where:** `src/denv.rs` (`env::set_var` and `env::remove_var` call sites)
- **Problem:** The comments claim a single-threaded process, but `foreground.rs` spawns worker threads. This overlaps ISH-15 and was left unverified by the comment cleanup.
- **Recommendation:** Resolve with ISH-15: state the real precondition at the call sites or remove the process-environment mutation.
- **Fix with:** os-boundaries
- **Effort:** S (hours)
