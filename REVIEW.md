# Review: ish

Scope: the `ish` binary and library, the `crates/ish-undo` crate, scripts, tests, Makefile, Dockerfile, and the release workflow at `1b3eafe`. Checks run: `cargo check --workspace --all-targets` passes; `cargo clippy --workspace --all-targets -- -D warnings` fails with 3 errors; PTY repros of R1, R2, and R3 against `target/debug/ish` with an isolated HOME. Other findings were reproduced by review lanes (PTY, fixture binary, or scratch crate) or read from code; those without a repro say `unproven`. Not covered: the full test suite (not run), benchmark gates (a lane's timings were taken on a loaded host and are indicative), Linux and musl behavior, the `epsh` dependency's source, and an actual release run.

## Summary

The undo crate is careful about ordering and replay, and the history log is a sound design, but the interactive shell around them trusts input it should not and does not match the contract it documents. Three themes cover most findings. First, untrusted text (`.env`, `.git/HEAD`, a host word at Tab) reaches a shell, the terminal, or a trusted script. Second, some undo and redirection paths fail open instead of closed. Third, `epsh`'s full grammar is exposed while README and AGENTS describe a smaller shell, and nothing runs tests or clippy before a change lands (clippy is already red).

## Findings

### R1 · P1 · `.env` files load without trust and their keys are spliced into trusted scripts

- **Area:** Security
- **Where:** `src/denv.rs:191-223`, `src/denv.rs:604-611`, `src/denv.rs:865-871`, `src/denv.rs:967-973`
- **Problem:** Only `.envrc` goes through `is_allowed`. In a directory with no `.envrc`, `cd` loads `.env` and sets any variable. Proven: `.env` with `PATH=/nonexistent` set `PATH` on `cd`, with no `denv allow`. With an allowed `.envrc`, a `.env` key is pasted into the generated script, so a line `A;touch X;B=1` runs the command. Proven: the marker file was created. A NUL byte in a value makes `set_var` panic, which kills the shell on `cd` (reproduced by a lane). The trust store location (`DENV_DATA_DIR`) is read from the same environment `.env` can write.
- **Recommendation:** Treat `.env` as untrusted: require the directory to be allowed before loading it, validate keys against `[A-Za-z_][A-Za-z0-9_]*`, reject NUL, and pass values through the environment instead of script text. Refuse loader-sensitive names (`LD_*`, `DYLD_*`, `PATH`) unless allowed.
- **Fix with:** make-invalid-states-unrepresentable
- **Effort:** S

### R2 · P1 · Tab completion of an ssh word executes shell metacharacters

- **Area:** Security
- **Where:** `src/complete.rs:835-895`, `src/main.rs:1832-1844`, `src/sys.rs:67-160`
- **Problem:** The text before the first `:` is interpolated unquoted into `/bin/sh -c "ssh ... {host} ..."`. Proven: typing `ssh a;touch${IFS}<path>:` then Tab created the file, with no Enter pressed. Pasted text makes this a trap. A leading `-o...` is option injection, ssh stderr lands on the screen, and remote file names enter the completion grid unsanitized. It also spawns a subprocess from completion, which AGENTS.md forbids.
- **Recommendation:** Validate the host against a strict `[user@]host` pattern that cannot start with `-`. Spawn `ssh` with an argv (`--` before the host, stderr to `/dev/null`) and no shell. Sanitize returned names as `ls.rs` does. Or remove remote-path completion (see Open decisions).
- **Fix with:** os-boundaries
- **Effort:** S

### R3 · P2 · No CI runs tests or clippy on changes, and clippy already fails on master

- **Area:** Build
- **Where:** `.github/workflows/release.yml:3-5` (only workflow, `workflow_dispatch` only), `src/complete.rs:395`, `src/render.rs:464`, `src/history.rs:867`, `Makefile:61-63`
- **Problem:** Tests run only inside a manual release job. Proven: `cargo clippy --workspace --all-targets -- -D warnings` fails with 3 errors (two argument-count lints, one `chunks_exact`). `make lint` cannot succeed because `--fix` cannot repair argument counts, and it is a mutating target (`cargo fmt` plus `clippy --fix --allow-dirty`) missing from `.PHONY`.
- **Recommendation:** Add a `pull_request`/`push` workflow that runs clippy `-D warnings` and `cargo test --lib --tests` on macOS and the Linux musl image. Fix the 3 lints. Make `lint` check-only and move mutation to a separate `fix` target.
- **Fix with:** renovate-tests
- **Effort:** M

### R4 · P2 · Redirections and native `rm`/`mv` fall back to unprotected execution when lookup fails or HOME is unset

- **Area:** Errors
- **Where:** `crates/ish-undo/src/ops.rs:1294-1298`, `src/undo.rs:70-73`, `src/undo.rs:160-171`, `src/undo.rs:223`
- **Problem:** The redirection provider returns `None` for "not applicable", and also for lookup errors, which makes epsh do its own truncating open with no journal. Proven: a directory of mode 0333 (writable, not readable) makes `>f` truncate `f` unprotected. With HOME unset or empty, redirections are unprotected silently, and `rm`/`mv` print a warning and run the external utility. AGENTS.md says native `rm`/`mv` fail closed with no silent fallback; the README says options fail "instead of silently running an unprotected utility".
- **Recommendation:** Return an explicit outcome (protected fd, not applicable, refused with error). Map lookup failures and a missing transaction to refusal. Make HOME-unset fail with a message naming `ISH_UNDO=off` as the way to opt out.
- **Fix with:** make-invalid-states-unrepresentable
- **Effort:** M

### R5 · P2 · The undo store guard does nothing until the store exists, and a deep `rm -r` cannot be undone

- **Area:** Data
- **Where:** `crates/ish-undo/src/ops.rs:128-130`, `ops.rs:151-162`, `ops.rs:773`, `ops.rs:753`, `crates/ish-undo/src/replay.rs:573-576`
- **Problem:** Two ways a protected `rm` loses data. (a) `Guard::new` skips the store when `stat` fails, and the store is created lazily. Proven with the fixture on a fresh store path: `rm -r` of an ancestor deleted into the new store, failed to seal, and left the preserved object unrecoverable. `mv` has the same hole. (b) Recorded paths are absolute byte strings and replay resolves parents by path. Proven on macOS: a 1456-byte path (PATH_MAX is 1024) removes with exit 0, then `undo` reports conflicts `File name too long` for the deepest entries.
- **Recommendation:** Guard the lexical store path and its existing ancestors even when the store is absent (or create the store before building the guard), with a test for the first operation on a fresh HOME. Refuse any removal whose recorded path exceeds PATH_MAX, or make replay descriptor-relative.
- **Fix with:** os-boundaries
- **Effort:** M

### R6 · P2 · denv trust compares only a one-second mtime

- **Area:** Security
- **Where:** `src/denv.rs:422-462`
- **Problem:** Proven by a lane: after `denv allow`, replacing `.envrc` contents and copying the old mtime with `touch -r` loaded the new file with no prompt. Archive extraction and `rsync -a` preserve mtimes. The check and the later evaluation are also separate steps.
- **Recommendation:** Store a content hash with the canonical path and verify it against the bytes actually evaluated. State the dependency cost if a hash crate is added; a small std-only SHA-256 is the alternative.
- **Fix with:** os-boundaries
- **Effort:** S

### R7 · P2 · ish runs epsh's full grammar and never reaps background children

- **Area:** Architecture
- **Where:** `src/main.rs:414`, `src/config.rs:62-74`, `src/signal.rs:30-33`, `README.md:350-359`
- **Problem:** README and AGENTS.md say there is no `&`, `if`/`for`, functions, `source`, or `eval`. Proven at the prompt: `sleep 3 & echo BGOK`, `if true; then ...; fi`, `f() { ...; }; f`, `source /dev/null`, `eval ...`, and `${HOME}` all work, and the backgrounded child stays `<defunct>` because nothing reaps it. The README's argument that "you can't `source` it" does not hold, and AGENTS.md says not to let epsh become a general execution path.
- **Recommendation:** Decide per Open decisions. To enforce the subset, walk the parsed AST before running and reject disallowed constructs and `init.sh`. Otherwise change the docs and reap children.
- **Fix with:** make-invalid-states-unrepresentable
- **Effort:** M

### R8 · P2 · First-word dispatch drops the rest of a list and builtins diverge inside lists

- **Area:** Architecture
- **Where:** `src/main.rs:260-394`, `src/main.rs:2541-2576`, `src/builtin.rs:7-23`
- **Problem:** Dispatch looks at the raw line's first word. Proven: `c && echo X`, `fg && echo X`, and `alias gg echo && echo X` run only the first command and drop the tail silently. `z nothing; echo X` passes `nothing;` to `z`. Inside a list or pipeline `w` runs `/usr/bin/w` and `z` is not found, while `which w` calls it a builtin. The builtin set is spelled in about five places (registry, dispatch match, handler, `undo.rs:157`, README) and they disagree; `Shell.prev_dir` is written and never read.
- **Recommendation:** Dispatch on the parsed command, or refuse a state-changing builtin that appears inside a list with a clear error. Keep one registry entry per builtin (name, execution class, handler) and derive `is_builtin`, completion, and `w` from it.
- **Fix with:** single-source-of-truth
- **Effort:** M

### R9 · P2 · History flattens newlines, so a recalled command differs from what ran

- **Area:** Data
- **Where:** `src/history.rs` (`add_in_dir`: `line.trim().replace('\n', " ")`)
- **Problem:** The log stores length-prefixed records and can hold newlines; the flattening is left from the old text format. Proven by a lane: `rm -rf build\nls` is stored as `rm -rf build ls`, and `echo a \\\nb` as `echo a \\ b`. Recalling a pasted multi-command block and pressing Enter runs a different, possibly destructive, command.
- **Recommendation:** Store the text verbatim and handle single-line display in the pager and `history` output.
- **Fix with:** durable-state
- **Effort:** M

### R10 · P2 · A damaged history log stops the shell from starting and has no recovery path

- **Area:** Data
- **Where:** `src/history/store.rs:174-258`, `src/main.rs:187-190`, `src/history/persistence_tests.rs:434`
- **Problem:** Proven by a lane with a scratch crate: appending 64 zero bytes (a size-extended torn write) or flipping a byte in the last frame is reported as corruption, not as a torn tail, and `main` exits 1. `history compact` and `history reset` read the log first, so both fail on the same error and the only way out is deleting files by hand, after which the stale legacy files are silently re-imported. A non-UTF-8 legacy `history` file aborts first-run import the same way.
- **Recommendation:** Treat a final frame that runs exactly to EOF with a bad checksum or zero fill as a torn tail. Add a loud salvage path that keeps the valid prefix and moves the remainder to `history.log.corrupt-*`. Make `reset` independent of the body. Test the zero-fill and torn-payload cases.
- **Fix with:** durable-state
- **Effort:** M

### R11 · P2 · The prompt writes untrusted `.git/HEAD` content and directory names raw to the terminal

- **Area:** Security
- **Where:** `src/prompt.rs:80-132`, `src/prompt.rs:205-232`, `src/main.rs:706-717`
- **Problem:** Proven in a PTY by a lane: a `.git/HEAD` containing `ref: refs/heads/main<ESC>]52;c;aGk=<BEL><ESC>[2J` made the prompt emit those bytes on `cd` into the repo, which is an OSC 52 clipboard write and a screen clear from a cloned checkout. Directory names with ESC or BEL are written raw to the prompt, and the OSC 7 URL percent-encodes only spaces. `display_len` understands only `ESC…m`, so prompt width drifts.
- **Recommendation:** Sanitize the branch and PWD as `ls.rs` does for names, percent-encode the OSC 7 path byte-wise, and compute width from the sanitized text.
- **Fix with:** os-boundaries
- **Effort:** S

### R12 · P2 · A mid-file bad journal record makes the next append truncate valid records after it

- **Area:** Data
- **Where:** `crates/ish-undo/src/journal.rs:1080-1156`, `journal.rs:967-1004`, `crates/ish-undo/tests/recovery.rs:187`
- **Problem:** `unproven` (traced by reading, no repro). `Writer::open` rescans from the header on first append and `ftruncate`s at the first record whose CRC or decode fails, not only at an incomplete tail frame. Every `undo`, `undo redo`, and seal appends, so a plain `undo` on a journal with one flipped bit mid-file destroys the later Prepare and Commit records it needs, orphaning their objects. The test covers only a damaged tail.
- **Recommendation:** Truncate only a genuine torn tail (an incomplete frame reaching EOF). If a complete frame fails validation, refuse to append, mark the transaction damaged, and keep the bytes. Add a flipped-byte-in-the-middle test.
- **Fix with:** durable-state
- **Effort:** M

### R13 · P2 · Retention treats interrupted transactions as permanent budget and deletes healthy history to compensate

- **Area:** Data
- **Where:** `crates/ish-undo/src/retention.rs:263-312`, `retention.rs:268-272`, `retention.rs:282-286`, `crates/ish-undo/src/txn.rs:314-339`, `crates/ish-undo/src/journal.rs:24-26`
- **Problem:** `unproven` (read, not run). A killed shell or failed seal leaves a transaction that is never sealed or collected, yet its bytes and count stay in the totals. Once over budget, every pass deletes all older completed transactions down to the newest while the interrupted ones keep their versions. Free space below `ISH_UNDO_MIN_FREE` (default 1 GiB) also triggers deletion, and the README documents it only as a copy reserve. Version 1 journals are cached at size 0, so they are never counted toward the budget or aged out. The session liveness lock is taken late and its failure is dropped (`.ok()`), so a live transaction can read as interrupted.
- **Recommendation:** Seal or collect interrupted transactions whose shell is dead and which are past the age limit. Compute "over budget" against collectable bytes only and stop when nothing more can be freed. Acquire the liveness lock at allocation and surface failure. Document the free-space rule.
- **Fix with:** durable-state
- **Effort:** M

### R14 · P2 · Interactive input can panic the shell, which aborts under `panic = "abort"`

- **Area:** Errors
- **Where:** `src/line.rs:303-345`, `src/frecency.rs:76`, `src/complete.rs:95`, `src/complete.rs:141`, `src/main.rs:1944-1947`, `src/denv.rs:604-611`
- **Problem:** Proven by lanes: with buffer `éa\nxyz`, Up from after `x` lands inside `é` and the next cursor computation panics; `z ''` panics with a zero window size whenever cd history exists; a partial-path completion name over 255 bytes truncates a length to `u8` and slices inside a character. Each kills the shell and the line being typed. Related to R1c (NUL in `.env`).
- **Recommendation:** Keep a display column in the line editor and map it to a char boundary, reject an empty `z` query, and widen the completion length or reject over-long entries. Add one regression test each.
- **Fix with:** error-model
- **Effort:** S

### R15 · P3 · Terminal is not restored on SIGTERM or panic, and a second Ctrl+Z orphans the first job

- **Area:** Concurrency
- **Where:** `src/term.rs:39-47`, `src/signal.rs:33-45`, `src/main.rs:2661`, `src/main.rs:426-437`, `src/denv.rs:891-922`
- **Problem:** Proven by a lane: `kill -TERM` of an idle ish left the pty non-canonical with echo off and bracketed paste on; restoration lives only in `Drop`, and there is no panic hook (release is `panic = "abort"`). Two Ctrl+Z in a row leave both processes in state `T` while ish tracks one, so the first can never be resumed. `.envrc` evaluation ignores Ctrl+C because the denv child inherits ignored SIGINT. A lane also found that `fg` does not continue a suspended AND-OR list as the README claimed; `resume_job` only waits.
- **Recommendation:** Add a panic hook and handle TERM/HUP through the existing self-pipe. Refuse to run while a job is stopped, or keep a list. Reset signal dispositions and add a timeout for the denv child.
- **Fix with:** lifecycle-concurrency
- **Effort:** M

### R16 · P3 · Undo bookkeeping swallows errors and store creation is not crash-safe

- **Area:** Errors
- **Where:** `crates/ish-undo/src/cli.rs:991`, `cli.rs:1025`, `cli.rs:662-668`, `crates/ish-undo/src/store.rs:113-121`, `store.rs:327-336`, `store.rs:538-543`, `crates/ish-undo/src/retention.rs:120-139`
- **Problem:** `unproven` (read). A transient registry read error during `volume add` rewrites the registry from empty, dropping other volumes; `volume remove` can unregister a volume an unreadable journal still uses. `purge` checks Active before taking the lock and does not re-check. `format` and `journal` are created non-atomically, so a crash during first use makes the store unreadable (fails closed until deleted by hand) and unreadable transactions are kept forever. Transaction deletion removes objects before the catalog entry, so a crash leaves a completed-looking transaction with missing objects.
- **Recommendation:** Propagate registry errors, create `format` and `journal` by write-temp-fsync-rename, rename a transaction to a tombstone before deleting it, and re-check liveness under the lock.
- **Fix with:** error-model
- **Effort:** M

### R17 · P3 · Release pipeline: static binary untested, toolchain pin and Docker inputs are uneven

- **Area:** Build
- **Where:** `.github/workflows/release.yml:76-86`, `rust-toolchain.toml:2`, `Dockerfile` (toolchain install, `rustup-init`, base image), `Makefile:9`
- **Problem:** `make test-ci` runs only against the dynamic Linux binary; the static binary that ships gets ELF checks only, and `src/signal.rs` records a musl static sigaction segfault. The nightly date is spelled in `rust-toolchain.toml` and the Dockerfile. LLVM is a release candidate (`23.1.0-rc2`) checksummed against build args, `rustup-init` has no checksum, and the base image is pinned by tag. The unstable flags are written in both the Makefile and the Dockerfile wrapper.
- **Recommendation:** Run `make test-ci` after the static build. Pass the channel from `rust-toolchain.toml` into the image, checksum `rustup-init`, and write a one-line toolchain bump note.
- **Fix with:** single-source-of-truth
- **Effort:** M

### R18 · P3 · Tests miss job control and signals, skip cross-filesystem undo silently, and rely on fixed read timeouts

- **Area:** Tests
- **Where:** `tests/pty.rs:3138`, `tests/pty.rs:336-357`, `tests/pty.rs:3016`, `crates/ish-undo/tests/stores.rs:25-40`, `src/main.rs` (0 unit tests in 2853 lines)
- **Problem:** One job-control scenario exists, with no coverage of Ctrl-C on a running child, Ctrl-Z of a pipeline, a second Ctrl-Z, exit with a stopped job, or SIGHUP/SIGTERM (R15 shows these break). Cross-filesystem undo tests return early unless `ISH_UNDO_TEST_VOLUME` is set, which CI does not set, so EXDEV and volume paths are never exercised. About 35 `read_timeout(N)` settles and two 1100ms sleeps risk flakes on loaded hosts; suite time and flake rate are `unproven`. `main.rs` dispatch is reachable only through PTY tests.
- **Recommendation:** Add 3-4 job-control and signal PTY scenarios, set the volume variable in the Linux job (or fail under `CI=1`), replace discarded reads with prompt barriers, and move the line-to-intercept-decision function into the library so it is unit-testable.
- **Fix with:** test-audit
- **Effort:** M

### R19 · P3 · Config errors are swallowed

- **Area:** Errors
- **Where:** `src/config.rs:29-37`, `src/config.rs:92-103`
- **Problem:** Proven by a lane: an invalid-UTF-8 default `config.ish` is dropped whole, including valid earlier aliases, with no message. `set 1BAD x` and an unterminated `set FOO "x` print nothing, and a bare `alias` line is reported as an unrecognized directive. `init.sh` runs only if `config.ish` exists.
- **Recommendation:** Report every read error other than not-found, surface the `set()` result, and reject lex failures instead of passing text through.
- **Fix with:** error-model
- **Effort:** S

### R20 · P3 · Dead weight: finished migrations, unused functions, and stale build entries

- **Area:** Dead code
- **Where:** `scripts/migrate-sqlite-history`, `src/history/store.rs:105-113`, `src/history/legacy.rs` (v1-v4), `scripts/import-fish-history`, `src/sys.rs:35` (`close_fds_from`, no callers), `Makefile:15`, `Makefile:67`, `.gitignore`, `.dockerignore`
- **Problem:** The SQLite history build lived 21 minutes on 2026-10-05 and is contained in no release tag (`git tag --contains 82c1b06` is empty), yet its migration script, guard, tests, and README paragraph remain. Cache formats v1-v4 date from March-April 2026. `import-fish-history` only works before the first start of the new build. `close_fds_from` has no callers. `search_all`, `fuzzy_search*` variants are used only by tests and sort differently from production `search_limited`, so ranking tests do not cover the shipped path. `Makefile` lists `setup` (undefined) in `.PHONY` and references `benches/fast-baseline.json`, which does not exist; `.gitignore` and `.dockerignore` list `fuzz/target`, `scripts/__pycache__/`, `benches/*-baseline.txt` and `Dockerfile.test`, none of which exist.
- **Recommendation:** After the owner confirms (Open decisions), delete the SQLite path and v1-v4 readers, remove `close_fds_from`, test-only search variants, and stale entries, and point the ranking tests at `search_limited`.
- **Fix with:** dead-code-archaeology
- **Effort:** S

### R21 · P3 · Ctrl+R search costs 24-27 ms per keystroke on a large history, and the PATH cache is stale and blocking

- **Area:** Performance
- **Where:** `src/history.rs` (`search_limited`, `compare`, `context_usage`), `benches/bench.rs`, `src/path.rs:44-60`, `src/path.rs:118`, `src/path.rs:176-183`, `src/path.rs:233`
- **Problem:** A lane measured, on a copy of a real log (54.6k occurrences) in a release build on a loaded host (indicative only), 26.8 ms for query `g` with cwd versus 4.1 ms without, because each comparison walks `cwd.ancestors()` with `PathBuf` hash lookups. The committed baseline (4.5 ms) uses synthetic entries with no directory data and hides this. The first non-builtin command word triggers a full PATH scan (42 ms warm), the cache refreshes only when the PATH string changes (a newly installed binary stays red until restart), and three predicates define "is executable" (one accepts sockets).
- **Recommendation:** Compute the directory score once per candidate and reject against the current worst result before inserting. Add a realistic-cwd bench and re-measure with `benchmark-checklist`. Use one executable predicate and scan PATH lazily after the prompt paints.
- **Fix with:** performance-by-mechanism
- **Effort:** M

### R22 · P3 · Ctrl+P / `ish-dump` files are world-readable and accumulate; history path fallbacks are unsafe

- **Area:** Security
- **Where:** `src/main.rs:589-619`, `src/history.rs` (`history_path_for_home`), `src/history/store.rs:92-98`
- **Problem:** Dumps in `~/.cache/ish/` are created with the default umask (observed `-rw-r--r--`) and contain the current line, history query, and completion names; typed secrets are exposed. They are never pruned. With `HOME=""` history is created relative to the launch directory, and with HOME unset it uses `/tmp/ish_history.log`, a shared predictable path opened without `O_NOFOLLOW` (symlink exposure `unproven`).
- **Recommendation:** Create the cache directory 0700 and dump files 0600, omit text fields unless asked, and fail loudly when HOME is unset or empty.
- **Fix with:** os-boundaries
- **Effort:** S

### R23 · P3 · Width and control-character handling in the editor

- **Area:** Architecture
- **Where:** `src/line.rs:447-472`, `src/input.rs:89`, `src/input.rs:136-150`, `src/main.rs:800-803`, `src/render.rs:136-146`
- **Problem:** Measured by a lane: `str_width("❤️")` is 3 and `str_width("👍🏽")` is 4, where terminals draw 2, so wrap and cursor math drift. A pasted tab is stored literally with width 0. Pasted control bytes 0x01-0x1a become the letters `a-z`. An oversized paste replaces the user's line with the text `[paste exceeded 8KB limit]`, which can then be run.
- **Recommendation:** Treat variation selectors, skin-tone modifiers, and ZWJ sequences as zero-width or combined, replace controls with a visible placeholder at the render boundary, and leave the line untouched on a rejected paste.
- **Fix with:** make-invalid-states-unrepresentable
- **Effort:** M

### R24 · P4 · Dependency and platform hygiene

- **Area:** Dependencies
- **Where:** `Cargo.toml`, `crates/ish-undo/Cargo.toml`, `src/sys.rs:42`, `src/sys.rs:92-150`, `crates/ish-undo/src/sys.rs:262-267`, `Makefile:75-111`
- **Problem:** `libc` and the rustix feature lists are declared in both manifests. `fxhash` may be flagged unmaintained (`unproven`; `epsh` also depends on it). Errno constants in the undo crate could use `rustix::io::Errno`. `posix_spawn` and the `close_range` syscall have no note on why rustix does not serve. `spawn_command_subst` leaks pipe fds and the spawn attributes on an early NUL return, and `close_fds_from` is a silent no-op on unsupported targets. `make install` changes `/etc/shells` and the login shell with sudo as part of installing a binary.
- **Recommendation:** Use `[workspace.dependencies]`, move errno handling to rustix, document the two libc sites, validate before allocating in the spawn helper, and split `install` into binary-only and an explicit `install-login-shell`.
- **Fix with:** dead-code-archaeology
- **Effort:** S

## Open decisions

- **Enforce the documented subset or change the docs (R7).** Option A: reject `&`, compound commands, function definitions, `source`, `eval`, `${VAR}`, and `init.sh` after parsing. Option B: document what the shell does and reap children. Recommendation: A, since the README and AGENTS.md describe a deliberately small shell and the "can't be tricked into sourcing" argument depends on it.
- **Keep ssh remote-path completion (R2).** Options: keep it with a strict host grammar and direct spawn, or remove it. Recommendation: remove it unless you use it, because it is the only completion that spawns a subprocess and runs on the UI thread for up to 3 seconds.
- **Delete the SQLite migration and the v1-v4 cache readers (R20).** Options: delete now, or keep until you confirm no other machine has a SQLite database or pre-July cache. Recommendation: delete, since no release tag contains the SQLite build.
- **What `history compact` is for (R9, R10).** It rewrites every occurrence and removes nothing. Options: add a retention rule that collapses old duplicates, or remove the subcommand. Recommendation: remove it and give `reset` and salvage a path that works on a damaged log.

## Strengths to keep

- Undo ordering: a version is saved and its record flushed before the change it protects, replay never discards newer data, and conflicts are reported and left untouched.
- `crates/ish-undo` has no dependency on ish internals, and `rm` walks by descriptor with `NOFOLLOW` and revalidates identity before each removal.
- The history log is append-only and checksummed with length-prefixed records and a lock file, and it syncs before a command runs.
- Release workflow actions are pinned by commit SHA, with `permissions: {}` at the top and per-job grants.
