Prefix: ISH
Next: 83

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
| [ISH-62](docs/rfc/ISH-62-capture-input-tail.md) | Keep unwritten input tail on converged capture relay; return on session end | Implemented; PTY input capture tests passed; debug `?` builtin retired under ISH-73 (converged relay/capture infrastructure retained for terminal handling and jobs) |
| [ISH-63](docs/rfc/ISH-63-login-shell-command.md) | Login shell `-c COMMAND` delegation to `/bin/sh -c`; `--config` for file | Implemented; non-interactive invocation tests passed |
| [ISH-73](#ish-73--p1--remove-debug-output-capture) | Remove the debugging-only `?` builtin and terminal-output capture rather than extending the capture architecture | Implemented 2026-11-09 (`4a24aac`); `?` builtin/tests/docs removed; PTY relay/capture infrastructure retained for converged terminal handling and suspended-job capture; filesystem quarantine unchanged |

## Interactive improvement campaign

Owner requested these TODOs on 2026-11-09. This is a backlog, not a claim that
any of the work below has shipped.

- Do not add `denv status`, `denv diff`, or other denv introspection work. Existing denv behavior is outside this campaign.
- Remove `?` and its terminal-output capture machinery instead of fixing or expanding split/converged capture. The proposed shell-owned previous-output browser is superseded by that removal: output navigation, search, selection, and copying belong to terminal integration (ISH-74 and ISH-80), not new shell-owned output storage.
- Keep ish interactive-only, with no scripting expansion, general plugin/completion framework, themes, configurable prompt language, daemon, or new general-purpose TUI framework. Preserve the single suspended-job limit and epsh ownership of child waits.
- Keep shell-owned completion, listing, and Git inspection in-process. Reuse epsh lexical/syntax information rather than introducing a second parser or an ish-owned AST. Editor inspection must not execute substitutions or commands.
- Record before/after prompt and completion latency, allocations, release size, and relevant resource behavior on matched workloads. Reuse existing benchmarks and buffers; require measured justification for dependencies, binary growth, or hot-path regressions. Never trade away execution, byte-preservation, or filesystem durability guarantees for convenience.
- Each implementation updates the relevant README contract and focused tests. Retain historical RFCs and qualify their status; do not erase evidence or claim model proofs establish untested terminal behavior.

Suggested delivery order: remove capture (ISH-73), add terminal integration
(ISH-74), improve completion (ISH-75), then history and editing (ISH-76/77).
Build the interoperability matrix (ISH-80) alongside the terminal changes.
Highlighting and picker consistency (ISH-78/79) follow the shared editor work.
Licensing and release identity (ISH-81/82) can proceed independently, with the
license choice reserved for the owner.

## Open implementation tickets

### ISH-73 · P1 · Remove debug output capture

- **Status:** Done (2026-11-09, `4a24aac`). Removed the debugging-only `?` builtin. The original acceptance called for removing capture-only PTY/relay machinery; that broader removal was not done because `src/capture.rs` still provides converged terminal handling and suspended-job capture used by normal execution/job control. Scope delivered: `?` command, handler, lookup/completion/dispatch exposure, dedicated tests, and user-facing docs removed; `copy-scrollback` and filesystem quarantine/journals unchanged.
- **Area:** Simplification / terminal correctness
- **Where:** `src/capture.rs`, `src/shell.rs`, `src/job.rs`, `src/builtin.rs`, `src/lib.rs`, relevant epsh foreground integration, `tests/pty.rs`, `README.md`, `docs/architecture.md`, capture RFCs and verification notes
- **Delivered:** `?` removed from dispatch, lookup, completion/help, builtin registry, and shell handling. `?`-specific PTY coverage/helpers removed; retained PTY coverage still exercises converged terminal behavior and large-output relay robustness. README/architecture updated. Focused checks: `cargo test --test integration builtin_is_builtin_known`, lib tests (288 passed; 1 pre-existing env failure in `history::persistence_tests::damaged_final_frame_is_a_torn_tail`), `cargo test --test pty --no-run`.
- **Follow-up:** If the project still wants full capture-relay retirement, that is separate work: it must preserve ordinary terminal inheritance/foreground handling, `/dev/tty` behavior, job control, and epsh contracts, and should be gated by ISH-80/native PTY evidence.
- **Effort:** M–L (builtin removal delivered; full relay retirement not attempted)

### ISH-74 · P1 · Add native terminal command integration

- **Status:** Implemented (2026-11-09); native-terminal matrix evidence remains under ISH-80.
- **Area:** Interactive UX / terminal protocol
- **Where:** `src/shell.rs`, `src/editor.rs`, `src/prompt.rs`, `src/term.rs`, `tests/pty.rs`, `README.md`
- **Delivered:** Emits OSC 133 A/B around each interactive prompt cycle, C before command execution, and D with exit status at completion. Cancelled input closes the prompt with D and no C; dispatch refusals emit D;2 with no C. Suspended commands emit D;148 and `fg` emits its own C/D around resume. Continuations stay in the same input session. Markers are emitted at runtime boundaries (not editor repaints) to the interactive terminal; redirected command output files and delegated `ish -c` do not receive markers. README documents terminal-owned capabilities/limits. Byte-level PTY assertions cover marker order/status, cancellation/refusal, resize non-duplication, continuation, suspend/fg, redirection isolation, and delegated `ish -c`.
- **Dependencies:** ISH-73 delivered the execution-path baseline; ISH-80 still owns native-terminal evidence (Ghostty/other terminals, SSH/tmux, completion/notification behavior where supported).
- **Effort:** S–M

### ISH-75 · P1 · Add argument-aware in-process completion

- **Status:** Planned.
- **Area:** Completion
- **Where:** `src/editor.rs`, `src/complete.rs`, `src/builtin.rs`, `src/path.rs`, epsh lexer/parser interfaces, `docs/completion.md`, editor/integration/PTY tests, `benches/bench.rs`
- **Recommendation:** Complete ish builtin options and operands, common command subcommands/options, and Git branches, tags, and remotes. Start with a finite native set, using builtin metadata as the source of truth where possible. Derive the command and argument position from reusable epsh lexical/syntax information instead of expanding `shell_tokens`, `split_whitespace`, and ad hoc command-position heuristics. Add a bounded tolerant inspection API in epsh if incomplete input requires one; do not build a second parser or evaluate the line. Read Git metadata in-process with explicit worktree/common-dir, loose/packed-ref, invalidation, and resource-bound behavior.
- **Acceptance:** Tests cover incomplete quotes, escapes, assignments, redirections, command lists/pipelines, aliases, supported wrappers, mid-line insertion, option arguments, and `--`. Suggestions preserve quoting and operand bytes, remain aligned with dispatch restrictions, and never execute substitutions. Git fixtures cover worktrees and changed refs without stale suggestions. Preserve existing path/host matching and ordering contracts unless an intentional change is documented. Benchmark cold/warm completion and large candidate sets; no subprocess-per-keystroke behavior, plugin ecosystem, or general completion configuration language.
- **Dependencies:** Shared syntax inspection should also serve ISH-78 and syntax-aware editing in ISH-77.
- **Effort:** M

### ISH-76 · P1 · Add selective history privacy and execution metadata

- **Status:** Planned; deliver privacy controls before optional ranking/UI changes.
- **Area:** History / privacy
- **Where:** `src/history.rs`, `src/history/store.rs`, `src/history/persistence_tests.rs`, `src/shell.rs`, `src/builtin.rs`, `src/editor.rs`, `docs/history-search.md`, history/integration/PTY tests
- **Recommendation:** Add documented leading-space history exclusion and selective `history forget` rather than requiring `history reset`. Detect exclusion on the original submitted input before trimming, alias expansion, or implicit-cd rewriting. Ensure excluded commands are not copied into other shell-owned command logs or diagnostic dumps as a side effect. Define explicit selection/identity semantics for forgetting an entry or its occurrences, including other active shells' search and recall snapshots. Retain the compact log architecture with deletion records and compaction as needed. Add versioned exit-status/outcome metadata so history can distinguish successful, failed, refused, interrupted, and unfinished commands; a stopped job is not a completed invocation.
- **Acceptance:** Private commands do not enter persisted history or later autosuggestions/search. Forgetting propagates without another shell resurrecting the deleted entry; migration, concurrent append/delete/reset, interrupted writes, and compaction have regression tests. Outcome updates preserve the distinction between pending/stopped and completed work, including `fg` and shell termination. Do not silently hide failed commands or change literal-first ranking. Document that exclusion/deletion is not secure disk erasure and does not stop invoked programs or the terminal from recording data.
- **Dependencies:** Coordinate runtime outcome recording with ISH-73/74; preserve existing session-snapshot semantics except explicit deletion propagation.
- **Effort:** M

### ISH-77 · P1 · Improve line editing and autosuggestion acceptance

- **Status:** Planned.
- **Area:** Editor UX
- **Where:** `src/line.rs`, `src/editor.rs`, `src/editor/tests.rs`, `src/input.rs`, `src/render.rs`, `tests/pty.rs`, `README.md`
- **Recommendation:** Add a bounded undo/redo history with deliberate edit grouping, word-by-word autosuggestion acceptance at the end of input, and more useful shell-word/path movement. Preserve Right-to-accept-the-whole-suggestion and existing cursor movement inside the line. Give typing runs, paste, completion acceptance, history recall, and alias expansion predictable undo boundaries; navigation/preview alone should not flood the undo history. Choose and document bindings without stealing Ctrl+Z from foreground job control.
- **Acceptance:** Deterministic editor tests and PTY tests cover undo/redo after paste and completion, multiline edits, cancelled pickers, suggestion acceptance one word at a time, and cursor movement around quoting and path separators. Maintain UTF-8 validity and display-width correctness, explicitly testing combining/wide-character cases and narrow/wrapped terminals. Bound retained memory and verify the warm editing path does not acquire avoidable per-keystroke allocations. Reuse shared lexical information where syntax matters rather than adding another tokenizer.
- **Dependencies:** Undo/redo can proceed independently; share syntax-aware movement work with ISH-75/78.
- **Effort:** M

### ISH-78 · P2 · Add parser-aware highlighting and diagnostics

- **Status:** Planned.
- **Area:** Editor feedback
- **Where:** `src/editor.rs`, `src/render.rs`, `src/dispatch.rs`, epsh lexer/parser interfaces, editor/PTY tests, `README.md`
- **Recommendation:** Extend command-existence coloring to shell words, quotes, operators, redirections, and unsupported/invalid syntax using the shared epsh inspection path. Distinguish incomplete input that legitimately needs continuation from a definite syntax/policy error. Validate commands at each command position rather than treating the entire line as one whitespace-delimited command. Keep the visual vocabulary fixed and small; no themes or configurable highlighter language.
- **Acceptance:** Highlighting and completion agree on spans and command positions for aliases, assignments, nested substitutions, multiline input, and supported operators. Inspection never evaluates substitutions, performs redirections, or mutates shell state; actual dispatch remains authoritative. Highlighted text cannot inject terminal controls. Snapshot/PTY coverage checks cursor geometry and Unicode/wrapping, and benchmarks quantify repaint/inspection cost.
- **Dependencies:** Reuse the tolerant lexical/syntax work from ISH-75 and integrate with ISH-77's edit model.
- **Effort:** M

### ISH-79 · P2 · Make interactive pickers consistent

- **Status:** Planned; does not include a captured-output browser.
- **Area:** Interactive UX / focused reuse
- **Where:** `src/editor.rs`, `src/mode.rs`, `src/render.rs`, `src/complete.rs`, history/directory-picker tests, `README.md`
- **Recommendation:** Make Ctrl+R history search, Tab completion, and Ctrl+Backspace directory selection a coherent family: consistent acceptance/cancellation, keyboard navigation, filtering/paste behavior, empty states, and resizing. Retain useful domain differences such as completion's grid and directory selection's navigation purpose. Share small layout/input helpers only where that removes duplication; do not introduce a generic TUI framework or change `fdu`'s independent browser contract gratuitously.
- **Acceptance:** A documented key/behavior matrix and focused tests show that cancellation restores the intended original input, previews do not execute commands, selection remains stable as results change, and resize/paste/multiline cases leave no stale rows or displaced cursor. Memory remains bounded and existing rendering caches/buffer reuse are preserved. Output search/selection stays terminal-owned under ISH-74 rather than adding a fourth picker backed by capture storage.
- **Dependencies:** Coordinate with ISH-75/77/78; preserve intentional existing paste/cancellation contracts unless explicitly revised.
- **Effort:** S–M

### ISH-80 · P1 · Establish a terminal interoperability regression matrix

- **Status:** In progress (2026-11-09); standing acceptance requirement, not a claim that PTY tests emulate real terminals completely.
- **Area:** Tests / terminal correctness
- **Where:** `tests/pty.rs`, `tests/external_signals.rs`, `tests/login_shell.rs`, `docs/ptytest.md`, [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md), `.github/workflows/ci.yml`, terminal integration documentation
- **Delivered so far:** Added [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md) separating deterministic PTY coverage from native/manual observations with host/environment records. Extended automated PTY coverage for OSC 7 emission, OSC 133 lifecycle (including cancel/refusal/continuation/suspend/fg/redirection isolation), delegated `ish -c` non-emission, bounded resize storms, and bounded repeated stop/resume. Recorded Ghostty 1.4.1-main (installed), tmux 3.8 (available; automated detached capture inconclusive), SSH localhost unavailable on this host as unverified/manual follow-up.
- **Remaining:** Isolated `/dev/tty` and fullscreen/alternate-screen PTY fixtures, explicit EOF/hangup and OSC 52 PTY assertions, broader leak/hung-wait/lost-input/terminal-restore stress checks, and reproducible native/manual evidence for Ghostty, an independent terminal, SSH sessions, and tmux (including prompt navigation/output boundaries and notification/completion behavior where supported). Unavailable native environments remain explicitly unverified, not passes.
- **Dependencies:** Develop alongside ISH-73/74 and use it as the acceptance gate for their terminal behavior.
- **Effort:** M

### ISH-81 · P2 · Resolve repository-wide licensing

- **Status:** Needs an explicit owner license choice before implementation; adding this TODO does not select or grant a license.
- **Area:** Project distribution
- **Where:** Repository root license/notice files, `README.md`, `Cargo.toml`, workspace crate manifests, imported crate license/import records
- **Recommendation:** Inventory the top-level project's ownership and the licenses/notices of bundled or imported code. Have the owner choose the license for code they are entitled to license, then add the appropriate root license and consistent package/readme metadata. Preserve imported notices and scope exceptions explicitly; do not infer a repository-wide MIT license from individual MIT-licensed crates or silently relicense third-party code.
- **Acceptance:** README, package metadata, root notices, and release contents clearly state the applicable license scopes and retain required attribution. Any unresolved ownership/license question remains visible rather than being guessed away.
- **Dependencies:** Owner decision; independent of the implementation campaign.
- **Effort:** S after owner decision

### ISH-82 · P3 · Give prereleases explicit version and revision identity

- **Status:** Planned; lower priority than interactive correctness and usability.
- **Area:** Release metadata
- **Where:** `Cargo.toml`, `src/main.rs`, `.github/workflows/release.yml`, `Makefile`, `README.md`, `docs/development.md`
- **Recommendation:** Define a lightweight development/prerelease version policy instead of leaving user-visible builds indefinitely identified only as `0.0.0`. Align package version, `ish -V`, release tags/names, and archive names, while retaining a source revision identifier for exact bug reports. Do not imply a stable API/behavior guarantee before one is intended, or turn this into a new release-management subsystem.
- **Acceptance:** A produced artifact can be traced unambiguously to its source revision and declared version. Local development builds and tagged prereleases report their identity consistently. Verify release metadata without weakening the existing verified-bytes, panic-diagnostic, static-Linux, or native-macOS release requirements.
- **Dependencies:** Coordinate published license metadata with ISH-81 when that decision lands; preserve ISH-42's release contract.
- **Effort:** S

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
