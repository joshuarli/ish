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
all of the work below has shipped.

- Do not add `denv status`, `denv diff`, or other denv introspection work. Existing denv behavior is outside this campaign.
- `?` and its terminal-output capture command were removed under ISH-73. Output navigation, search, selection, and copying belong to terminal integration (ISH-74 and ISH-80), not new shell-owned output storage.
- Keep ish interactive-only, with no scripting expansion, general plugin/completion framework, themes, configurable prompt language, daemon, or new general-purpose TUI framework. Preserve the single suspended-job limit and epsh ownership of child waits.
- Keep shell-owned completion, listing, and Git inspection in-process. Reuse epsh lexical/syntax information rather than introducing a second parser or an ish-owned AST. Editor inspection must not execute substitutions or commands.
- Record before/after prompt and completion latency, allocations, release size, and relevant resource behavior on matched workloads. Reuse existing benchmarks and buffers; require measured justification for dependencies, binary growth, or hot-path regressions. Never trade away execution, byte-preservation, or filesystem durability guarantees for convenience.
- Each implementation updates the relevant README contract and focused tests. Retain historical RFCs and qualify their status; do not erase evidence or claim model proofs establish untested terminal behavior.

Suggested delivery order: complete terminal interop gaps (ISH-80 actionable
parts), improve completion (ISH-75), then editing (ISH-77). Highlighting and
picker consistency (ISH-78/79) and prerelease identity (ISH-82) are lower
priority.

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

### ISH-80 · P1 · Establish a terminal interoperability regression matrix

- **Status:** Deterministic PTY gaps addressed (2026-11-09); native/manual matrix remains a standing follow-up.
- **Area:** Tests / terminal correctness
- **Where:** `tests/pty.rs`, `tests/external_signals.rs`, `tests/login_shell.rs`, `docs/ptytest.md`, [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md), `.github/workflows/ci.yml`, terminal integration documentation
- **Delivered:** [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md) separates deterministic PTY coverage from native/manual observations. Automated PTY coverage includes OSC 7, OSC 133 lifecycle (order/status, cancel/refusal, continuation, suspend/fg, redirection isolation), delegated `ish -c` non-emission, bounded resize storms, bounded repeated stop/resume, single-command `/dev/tty` input (`converged_command_reads_from_dev_tty`), alt-screen sequence relay passthrough with shell responsiveness (`alternate_screen_sequences_pass_through_relay`), raw OSC 52 clipboard emission for `copy-scrollback` (`osc52_copy_scrollback_emits_base64_session_log`), and SIGHUP-at-prompt exit/restore (`shell_sighup_at_prompt_exits_and_restores_terminal`).
- **Remaining:** Reproducible native/manual evidence for Ghostty, an independent terminal, SSH sessions, and tmux (prompt navigation/output boundaries, notification/completion behavior where supported), plus focused instrumentation/native runs for leaked descriptors/children, hung waits, lost input, terminal-mode restoration across native environments, and external child hangup/EOF edge cases. Unavailable native environments remain explicitly unverified, not passes.
- **Dependencies:** Acceptance gate for ISH-73/74 terminal behavior; retain as standing evidence, not a claim that PTY emulates real terminals completely.
- **Effort:** M

### ISH-75 · P1 · Add argument-aware in-process completion

- **Status:** Planned.
- **Area:** Completion
- **Where:** `src/editor.rs`, `src/complete.rs`, `src/builtin.rs`, `src/path.rs`, epsh lexer/parser interfaces, `docs/completion.md`, editor/integration/PTY tests, `benches/bench.rs`
- **Recommendation:** Complete ish builtin options and operands, common command subcommands/options, and Git branches, tags, and remotes. Start with a finite native set, using builtin metadata as the source of truth where possible. Derive the command and argument position from reusable epsh lexical/syntax information instead of expanding `shell_tokens`, `split_whitespace`, and ad hoc command-position heuristics. Add a bounded tolerant inspection API in epsh if incomplete input requires one; do not build a second parser or evaluate the line. Read Git metadata in-process with explicit worktree/common-dir, loose/packed-ref, invalidation, and resource-bound behavior.
- **Acceptance:** Tests cover incomplete quotes, escapes, assignments, redirections, command lists/pipelines, aliases, supported wrappers, mid-line insertion, option arguments, and `--`. Suggestions preserve quoting and operand bytes, remain aligned with dispatch restrictions, and never execute substitutions. Git fixtures cover worktrees and changed refs without stale suggestions. Preserve existing path/host matching and ordering contracts unless an intentional change is documented. Benchmark cold/warm completion and large candidate sets; no subprocess-per-keystroke behavior, plugin ecosystem, or general completion configuration language.
- **Dependencies:** Shared syntax inspection should also serve ISH-78 and syntax-aware editing in ISH-77.
- **Effort:** M

### ISH-77 · P1 · Improve line editing and autosuggestion acceptance

- **Status:** Planned.
- **Area:** Editor UX
- **Where:** `src/line.rs`, `src/editor.rs`, `src/editor/tests.rs`, `src/input.rs`, `src/render.rs`, `tests/pty.rs`, `README.md`
- **Recommendation:** Add a bounded undo/redo history with deliberate edit grouping, word-by-word autosuggestion acceptance at the end of input, and more useful shell-word/path movement. Preserve Right-to-accept-the-whole-suggestion and existing cursor movement inside the line. Give typing runs, paste, completion acceptance, history recall, and alias expansion predictable undo boundaries; navigation/preview alone should not flood the undo history. Choose and document bindings without stealing Ctrl+Z from foreground job control.
- **Acceptance:** Deterministic editor tests and PTY tests cover undo/redo after paste and completion, multiline edits, cancelled pickers, suggestion acceptance one word at a time, and cursor movement around quoting and path separators. Maintain UTF-8 validity and display-width correctness, explicitly testing combining/wide-character cases and narrow/wrapped terminals. Bound retained memory and verify the warm editing path does not acquire avoidable per-keystroke allocations. Reuse shared lexical information where syntax matters rather than adding another tokenizer.
- **Dependencies:** Undo/redo can proceed independently; share syntax-aware movement work with ISH-75/78.
- **Effort:** M

### ISH-78 · P3 · Add parser-aware highlighting and diagnostics

- **Status:** Planned (lower priority).
- **Area:** Editor feedback
- **Where:** `src/editor.rs`, `src/render.rs`, `src/dispatch.rs`, epsh lexer/parser interfaces, editor/PTY tests, `README.md`
- **Recommendation:** Extend command-existence coloring to shell words, quotes, operators, redirections, and unsupported/invalid syntax using the shared epsh inspection path. Distinguish incomplete input that legitimately needs continuation from a definite syntax/policy error. Validate commands at each command position rather than treating the entire line as one whitespace-delimited command. Keep the visual vocabulary fixed and small; no themes or configurable highlighter language.
- **Acceptance:** Highlighting and completion agree on spans and command positions for aliases, assignments, nested substitutions, multiline input, and supported operators. Inspection never evaluates substitutions, performs redirections, or mutates shell state; actual dispatch remains authoritative. Highlighted text cannot inject terminal controls. Snapshot/PTY coverage checks cursor geometry and Unicode/wrapping, and benchmarks quantify repaint/inspection cost.
- **Dependencies:** Reuse the tolerant lexical/syntax work from ISH-75 and integrate with ISH-77's edit model.
- **Effort:** M

### ISH-79 · P3 · Make interactive pickers consistent

- **Status:** Planned (lower priority); does not include a captured-output browser.
- **Area:** Interactive UX / focused reuse
- **Where:** `src/editor.rs`, `src/mode.rs`, `src/render.rs`, `src/complete.rs`, history/directory-picker tests, `README.md`
- **Recommendation:** Make Ctrl+R history search, Tab completion, and Ctrl+Backspace directory selection a coherent family: consistent acceptance/cancellation, keyboard navigation, filtering/paste behavior, empty states, and resizing. Retain useful domain differences such as completion's grid and directory selection's navigation purpose. Share small layout/input helpers only where that removes duplication; do not introduce a generic TUI framework or change `fdu`'s independent browser contract gratuitously.
- **Acceptance:** A documented key/behavior matrix and focused tests show that cancellation restores the intended original input, previews do not execute commands, selection remains stable as results change, and resize/paste/multiline cases leave no stale rows or displaced cursor. Memory remains bounded and existing rendering caches/buffer reuse are preserved. Output search/selection stays terminal-owned under ISH-74 rather than adding a fourth picker backed by capture storage.
- **Dependencies:** Coordinate with ISH-75/77/78; preserve intentional existing paste/cancellation contracts unless explicitly revised.
- **Effort:** S–M

### ISH-82 · P3 · Give prereleases explicit version and revision identity

- **Status:** Planned (lower priority).
- **Area:** Release metadata
- **Where:** `Cargo.toml`, `src/main.rs`, `.github/workflows/release.yml`, `Makefile`, `README.md`, `docs/development.md`
- **Recommendation:** Define a lightweight development/prerelease version policy instead of leaving user-visible builds indefinitely identified only as `0.0.0`. Align package version, `ish -V`, release tags/names, and archive names, while retaining a source revision identifier for exact bug reports. Do not imply a stable API/behavior guarantee before one is intended, or turn this into a new release-management subsystem.
- **Acceptance:** A produced artifact can be traced unambiguously to its source revision and declared version. Local development builds and tagged prereleases report their identity consistently. Verify release metadata without weakening the existing verified-bytes, panic-diagnostic, static-Linux, or native-macOS release requirements.
- **Dependencies:** Coordinate published license metadata with the root MIT license; preserve ISH-42's release contract.
- **Effort:** S

### ISH-45 · P3 · Native macOS privileged ownership acceptance remains incomplete

- **Status:** The accepted restrictive-umask boundary is implemented. Ordinary-user macOS workspace tests, `make test`, and strict workspace Clippy passed. Only privileged ownership acceptance remains. Re-checked: `clone_declines_inherited_read_denial_before_creation` and `clone_declines_before_creation_when_owner_read_is_absent` still take the root branch only when `geteuid` is 0, and `private_directory_prepares_foreign_owned_staging_directory` is still ignored.
- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_macos.rs` (`clone_declines_inherited_read_denial_before_creation`, `clone_declines_before_creation_when_owner_read_is_absent`, `private_directory_prepares_foreign_owned_staging_directory`), `docs/filesystem-testing.md`
- **Problem:** The foreign-owner clone branches and staging case require native root access to change ownership and drop credentials. Noninteractive sudo still requires a password on this host, so ordinary-user passes do not establish those branches.
- **Recommendation:** Run the two clone-decline cases as root and the ignored foreign-owned staging case using the commands in the filesystem testing guide. Close this ticket only after those native branches pass. The implementation and support-boundary decisions are settled.
- **Fix with:** none
- **Effort:** S (hours)

## Won't do

### ISH-76 · Selective history privacy and execution metadata

- **Status:** Won't do.
- **Reason:** History is a plain file under the user's control (`~/.local/share/ish/history.log`); sensitive entries can be removed by editing the history file. The added complexity of leading-space exclusion, cross-shell deletion propagation, compaction/delete records, and versioned execution metadata is not justified here.

### ISH-60 · Journaled retirement makes large flat EXDEV moves take minutes

- **Status:** Dropped.
- **Reason:** Parked candidate exists but timing proof requires quiet-host, interleaved baseline/candidate measurement (weeks-scale effort). Not worth pursuing now.

### ISH-81 · Resolve repository-wide licensing

- **Status:** Done (2026-11-09).
- **Resolution:** Added root [LICENSE](LICENSE) (MIT) for the top-level `ish` project, set `license = "MIT"` in [Cargo.toml](Cargo.toml), and updated [README.md](README.md#help-and-license). Existing bundled/imported crate and fixture license/notice files are retained (for example `ish-fs` LICENSE/IMPORT.md, `epsh` conformance NOTICE, fdu crate licenses).
