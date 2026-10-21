Prefix: ISH
Next: 45

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

### ISH-29 · P2 · ish-fs native boundary tests print SKIP and pass

- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_boundaries.rs:52-53`, `:63-64`, `:153-154`, `:250-256`, `:309`, `:336`, `:416`, `crates/ish-fs/tests/native_macos.rs:120`, `:155`, `:183`, `:296`, `benches/fs/native-fixtures.sh:85-113`
- **Problem:** Cross-device, nested-mount, alias and foreign-owner cases return early when `ISH_FS_TEST_*` variables are unset, and `cargo test` hides stderr for passing tests. Only `native-fixtures.sh` sets those variables; `rg ISH_FS_TEST Makefile .github Dockerfile` finds nothing. `ish-fs` is a default member, so `make test` and CI report green for the checks that guard cross-device `mv` retirement. The macOS skips on hosts without APFS are unproven.
- **Recommendation:** Mark each environment-dependent case `#[ignore = "reason"]` and run it from the fixture script, or fail when the platform requires the fixture. A missing precondition must not read as a pass.
- **Fix with:** test-audit
- **Effort:** S (hours)

### ISH-15 · P2 · Safe library APIs conceal environment mutation preconditions

- **Area:** Security
- **Where:** `src/lib.rs`, `src/denv.rs`
- **Problem:** Public safe wrappers call unsafe std environment mutation without enforcing its Unix process-wide concurrency preconditions. Public denv operations also reach those mutations. Safe library callers can invoke them while another thread uses libc environment readers. The pinned standard-library safety contract explicitly names such readers, including DNS resolution. This is a source-proven missing safety boundary; a concrete undefined-behavior manifestation is unproven and was not attempted. Normal ish foreground execution and joined relay threads do not establish an observed incident.
- **Recommendation:** Expose the required safety contract through every public mutation entry point, or remove process-environment mutation from the library interface in favor of explicit environments. Changing only the three wrappers leaves the denv boundary unresolved.
- **Fix with:** os-boundaries, api-surface-and-module-boundaries
- **Effort:** M (days)

### ISH-30 · P3 · Linux metadata copy applies mode before xattrs, so read-only files with `user.*` xattrs fail

- **Area:** Data
- **Where:** `crates/ish-fs/src/platform/linux.rs:845-856`, `crates/ish-fs/tests/transfer.rs:69`, `crates/ish-fs/tests/native_linux.rs:346`
- **Problem:** `apply_fd_metadata` runs `fchmod` with the final mode and then `fsetxattr`. The review lane ran `setxattr` as uid 1000 on this ext4 volume after `chmod 444`; it returned EACCES for both path and fd. A 0444 or 0555 source carrying a `user.*` xattr therefore fails metadata application and the transfer reports failure with the source retained, against the preserved-xattr and read-only-mode contract in `docs/filesystem.md`. No test combines the two. The ish-fs path itself was not run end to end.
- **Recommendation:** Apply xattrs and ACL before the final `fchmod`. Add one test with a 0444 file carrying a `user.*` xattr.
- **Fix with:** os-boundaries
- **Effort:** S (hours)

### ISH-31 · P3 · `--replace` checks the destination and then renames over it

- **Area:** Data
- **Where:** `crates/ish-fs/src/engine/transfer.rs:147-149`, `crates/ish-fs/src/engine/mod.rs:297-300`, `docs/filesystem.md`
- **Problem:** Replace calls `require_unchanged(destination)` and then `renameat` over the name. An object created or swapped in between is destroyed unobserved. Other publish paths use `RENAME_NOREPLACE`; replace is the one place that can lose an object the engine never saw.
- **Recommendation:** Exchange atomically (`RENAME_EXCHANGE` on Linux, `RENAME_SWAP` on macOS), verify the displaced object matches the observed destination, and restore it on mismatch.
- **Fix with:** os-boundaries
- **Effort:** M (days)

### ISH-32 · P3 · Two capture-and-retire protocols and duplicated identity code across `fdu-*` and `ish-fs`

- **Area:** Architecture
- **Where:** `crates/fdu-delete/src/posix/quarantine.rs`, `crates/ish-fs/src/engine/retirement.rs:58-100`, `crates/ish-fs/src/engine/mod.rs:31`, `crates/fdu-delete/src/lib.rs:783-834`, `crates/fdu-scan/src/fsutil.rs:20`, `crates/ish-fs/src/roots.rs:18`, `docs/filesystem.md:210-217`
- **Problem:** Both crates implement rename to a hidden name, verify identity, unlink, restore on mismatch. Only `fdu-delete` is journaled and recovers on the next run; ish-fs leaves `.ish-fs-retired-*`, `.ish-fs-intent-*` and `.ish-fs-stage-*` names after SIGKILL or power loss with no sweep, and names built from pid plus a counter can collide with an orphan under pid reuse. `FileIdentity` and `Identity`, four mount-identity helpers, three EINTR retry loops and two macOS `fsid_t` `transmute_copy` calls exist in parallel, each with its own `cfg` and fixes.
- **Recommendation:** Choose one capture protocol and one identity and mount module. Until then, have ish-fs report orphan `.ish-fs-*` entries at operation start and use random names.
- **Fix with:** single-source-of-truth, durable-state
- **Effort:** L (weeks)

### ISH-33 · P3 · Unused scan engine and test-only API ship in production code

- **Area:** Dead code
- **Where:** `crates/fdu-scan/src/lib.rs:177-190`, `crates/fdu-scan/src/linux.rs:346-890`, `crates/fdu-scan/src/macos.rs`, `crates/fdu-scan/src/exclusions.rs`, `src/denv.rs:1474-1489`, `src/prompt.rs:105-118`
- **Problem:** The only consumer of `fdu-scan` is `src/trash.rs:3`, which imports `RootAnchor`, `open_root_no_follow` and `start_indexed_scan`. `rg` finds no use of `scan`, `scan_with_exclusions`, `ScanOptions` or `ScanExclusions` outside the crate, leaving about 2k lines, a second traversal implementation and its unsafe code and dependencies. `apply_bash_output_bench` has no caller outside its unit test, and `Prompt::render` is labelled legacy and used only by tests. `indexed.rs` shares platform helpers with the dead code, so removal needs care.
- **Recommendation:** Delete the one-shot scan entry points, exclusions and unused wrappers, then prune dependencies. Delete `apply_bash_output_bench` and rename or delete the legacy prompt render.
- **Fix with:** dead-code-archaeology
- **Effort:** M (days)

### ISH-34 · P3 · `main.rs` holds the loop, editor, spawn path and diagnostics in one 3000-line file

- **Area:** Architecture
- **Where:** `src/main.rs:771-1306`, `src/main.rs:146-382`, `src/main.rs:518-725`, `src/main.rs:1692-2160`, `src/main.rs:2669-2830`, `src/lib.rs:32`
- **Problem:** `read_line_with_reader` is 535 lines and the file mixes the event loop, completion tokenizing, the layout-dump diagnostic with its `/dev/urandom` name generator, base64, `cd` rewriting and external spawning with job handling. The keystroke path has no unit seam; `main_history_tests.rs` is 22 lines. `static_word` is compiled twice, once in the library and once as a binary module. `/dev/urandom` failure falls back silently to a time-seeded value for the dump file name (`src/main.rs:701-724`).
- **Recommendation:** Move the external handler, spawn and job wait into one library module, the dump and base64 helpers into a diagnostics module, and split the input function per mode. Declare each module once. Fail the dump name on randomness errors.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** L (weeks)

### ISH-35 · P3 · Command policy facts are written in several places

- **Area:** Architecture
- **Where:** `src/builtin.rs:29-48`, `src/main.rs:384-516`, `src/main.rs:2669-2720`, `src/dispatch.rs:81-91`, `src/dispatch.rs:142-151`, `crates/epsh/src/builtins.rs:1273-1278`
- **Problem:** AGENTS.md says each builtin is registered once in `builtin.rs`. The class lives there, but behaviour is matched again by raw name in `run_shell_builtin` and the external handler, so a table entry compiles without a handler arm and a `Native` entry with no arm falls through to a PATH lookup. `plan()` and the dynamic check each hardcode `"eval" | "source" | "."` and the InShell lookup with different rejection variants, and epsh's `BUILTIN_NAMES` and `try_builtin` arms are a third copy with no test tying them together.
- **Recommendation:** Give each registry entry a typed handler per class and resolve all dispatch through `lookup()`. Add one `refused_command_name` shared by both policy checks and a test that every epsh builtin is allowed or refused.
- **Fix with:** single-source-of-truth
- **Effort:** M (days)

### ISH-36 · P3 · Library and epsh public surfaces are wider than their one consumer

- **Area:** Architecture
- **Where:** `src/lib.rs:1-34`, `src/sys.rs:7`, `src/job.rs:13-17`, `crates/epsh/src/lib.rs:40-60`, `crates/epsh/tests/api_stability.rs`
- **Problem:** The ish library exports every module as `pub`, including unsafe or ambient-descriptor helpers such as `continue_in_foreground`, although the only consumer is the binary and its tests. About 30 `BorrowedFd::borrow_raw(0 or 1)` calls repeat across five files. epsh exports `arith`, `glob`, `encoding`, `var`, `lexer`, `expand` and `builtins`; nothing in `src` uses `epsh::arith`, `epsh::glob` or `epsh::encoding`, and `api_stability.rs` freezes the whole surface, so ish-driven changes look breaking. How much of it is dead was not enumerated.
- **Recommendation:** Make unused modules `pub(crate)`, add `term::stdin()` and `term::stdout()` borrowed-fd accessors, and shrink `api_stability.rs` to the supported embedding API. If epsh is meant as a general crate, say so in its README.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** M (days)

### ISH-37 · P3 · A child that stops or dies in raw mode may leave its terminal state as the shell's baseline

- **Area:** Operability
- **Where:** `src/main.rs:2808-2810`, `src/term.rs:35-46`, `src/term.rs:56-67`, `src/job.rs:14-16`
- **Problem:** `RawMode::enable` captures whatever termios is current as `orig`, and `drop` restores it before the next command. `STARTUP_TERMIOS` is used only by the panic hook. After a child stops or is killed while holding raw mode, the next prompt's `orig` is that raw state and later commands run under it. Traced from code, not executed in a PTY; programs such as vim restore termios themselves on SIGTSTP, which lowers likelihood.
- **Recommendation:** Capture the baseline once at startup, apply it after every foreground wait in `spawn_external` and `resume_job`, and restore a job's own saved termios only for `fg`.
- **Fix with:** os-boundaries
- **Effort:** S (hours)

### ISH-38 · P3 · Foreground wait, handoff and signal paths drop errors

- **Area:** Errors
- **Where:** `src/main.rs:2770-2790`, `src/main.rs:2808-2809`, `src/main.rs:2551-2571`, `src/job.rs:14-45`, `src/signal.rs:4-25`, `src/signal.rs:166-170`, `src/foreground.rs:238`
- **Problem:** `waitpid(...).ok()` turns ECHILD into a silent status 1; `tcsetpgrp`, `setpgid` and `tcsetattr` results are discarded; a stopped child whose `save_termios` fails reaches `expect("terminal attributes unavailable")` and aborts the shell. `resume_job` takes the job out of the slot and drops it on any wait error. `exit 3` with a suspended job and the warning already shown exits 0. The signal self-pipe is `static mut` under a SAFETY comment claiming one thread while `Foreground` spawns workers, and the handler does not preserve errno.
- **Recommendation:** Report wait and handoff errors with a defined status, put the job back on error, carry the exit status through `ReadResult`, make the pipe descriptors atomics and save errno in the handler.
- **Fix with:** error-model, lifecycle-concurrency
- **Effort:** M (days)

### ISH-39 · P3 · Deleting `history.log` brings back the legacy history, which is never retired

- **Area:** Data
- **Where:** `src/history/store.rs:102-113`, `src/history/store.rs:172-192`, `src/history/legacy.rs:35-58`
- **Problem:** `Store::open` re-imports the legacy text and `.bin` cache whenever `history.log` is missing, and nothing removes them after migration. A user who deletes the log to clear history, or a crash between `reset` renaming a bad log and writing its replacement, resurrects old commands that may contain secrets.
- **Recommendation:** Rename or delete the legacy files after a successful import, and write the replacement before moving the old log aside.
- **Fix with:** durable-state
- **Effort:** S (hours)

### ISH-40 · P3 · `z` scores relative `cd` targets without the directory they ran in

- **Area:** Data
- **Where:** `src/frecency.rs:50-65`, `src/frecency.rs:87`, `src/frecency.rs:110-125`
- **Problem:** History stores each command's cwd, but `z` scores only the command text, so `cd src` or `cd ..` counts as the literal `src` or `..` and is resolved against the current directory, which can jump somewhere unrelated. Every history entry is also parsed with epsh on each `z` call. Cost was not measured.
- **Recommendation:** Resolve each target against its stored cwd or ignore relative targets, and prefilter on the first word before parsing.
- **Fix with:** none
- **Effort:** M (days)

### ISH-41 · P3 · The default test commands do not cover what ships

- **Area:** Tests
- **Where:** `Makefile:58-61`, `tests/pty.rs:61`, `tests/external_signals.rs:64`, `tests/history_search_paste.rs:35`, `Cargo.toml:15`, `crates/fdu-delete/src/posix/quarantine.rs:247`, `tests/fs_bench.rs:29`, `tests/pty.rs:36-38`
- **Problem:** `make test-ci` sets `ISH_TEST_BINARY`, but only `pty.rs` reads it; the signal and paste suites run the test-profile binary, and none of the tests use the build-std, `-Cpanic=immediate-abort` binary that is released. `default-members` omits `fdu-delete`, whose three tests guard `t`'s destructive path and run only under `--workspace`. `tests/fs_bench.rs` is 1,702 lines of ignored benchmark driver under `tests/`. The PTY suites use `temp_dir()` instead of `.work/tmp`.
- **Recommendation:** Resolve the binary through one helper used by all three suites, add `fdu-delete` to `default-members`, move the benchmark driver under `benches/fs`, and base test scratch on `.work/tmp`.
- **Fix with:** renovate-tests
- **Effort:** M (days)

### ISH-42 · P3 · Release delivery lacks a ref guard and integrity checks, and release panics are silent

- **Area:** Build
- **Where:** `.github/workflows/release.yml:4`, `:22`, `:110-116`, `scripts/check-refinement.py:26-29`, `:48-52`, `:89-93`, `Makefile:11`, `Dockerfile:124`, `Cargo.toml:51`
- **Problem:** `workflow_dispatch` publishes a pre-release from any branch with a zip only, no checksum, and the build job holds `actions: write` with no visible use. Rollback is deleting `release-<sha>` by hand. `check-refinement.py` runs an Aeneas tarball and `rustup-init` downloaded without digests, while the Dockerfile verifies equivalents with `sha256sum`. Release uses `-Cpanic=immediate-abort`, which skips the panic hook, so a panic ends in SIGABRT with no message; not run, inferred from the flag.
- **Recommendation:** Decide whether dispatch is limited to the default branch and whether artifacts need checksums, drop unneeded `actions: write`, pin digests in the refinement script, and decide whether the size gain justifies silent aborts, documenting the outcome.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-43 · P3 · History search allocates on every key

- **Area:** Performance
- **Where:** `src/main.rs:881`, `src/main.rs:2161-2182`, `src/main.rs:2177`, `src/main.rs:2308`
- **Problem:** AGENTS.md requires reusing buffers on the keystroke path. `handle_history_search_key` calls `query.text().to_string()` on every key, the paste path does the same, and `update_history_search` moves the candidate vector into the stack with `mem::take`, leaving an empty vector that regrows. Not measured.
- **Recommendation:** Keep one reusable previous-query buffer in the search mode and recycle popped candidate vectors, then confirm with the existing benchmark.
- **Fix with:** make-it-fast
- **Effort:** S (hours)

### ISH-44 · P3 · About 900 comments contradict the repo's no-comment contract and excuse code smells

- **Area:** Navigability
- **Where:** `rg '^\s*//'` counts: `src/main.rs` 166, `src/render.rs` 63, `src/line.rs` 72, `src/input.rs` 41, `src/dispatch.rs` 29, `src/signal.rs` 28, `crates/epsh/src/parser.rs` 68, `crates/epsh/src/eval.rs` 57, `crates/epsh/src/lexer.rs` 54, `src/history/store.rs:76-77,152-153,182-183,317,344,370-371`, `src/denv.rs:1049,1074,1080,2460`
- **Problem:** Counts include allowed SAFETY and API comments, so the violating share was not separated. Worst groups: false SAFETY claims (`src/signal.rs:4-8`, `:22-24` say single-threaded; `src/term.rs:42-45` is a SAFETY comment on a safe call); design rationale that belongs in types or tests (`src/main.rs:20-22`, `:2674`, `:2713`, `src/history/store.rs`); epsh comments citing line numbers of an external dash tree (`crates/epsh/src/eval.rs:1389-1393`, `crates/epsh/src/lexer.rs:700`, `:826`); and narration. `crates/fdu-delete` and `crates/ish-fs` are clean.
- **Recommendation:** Delete each group and fix the code it excused, starting with the false SAFETY claims. Move behaviour rules into test names.
- **Fix with:** comment-cleanup
- **Effort:** M (days)
