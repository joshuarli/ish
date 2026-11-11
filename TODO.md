Prefix: ISH
Next: 92

## Accepted owner decisions

The [accepted boundary records](docs/architecture.md#accepted-boundary-decisions)
remain implementation contracts. Retain those RFCs after delivery, as required
by this repository. Open work and verification limits are listed below.

## Interactive improvement constraints

- Keep ish interactive-only, with no scripting expansion, plugins, themes, configurable prompt language, daemon, or general-purpose TUI framework. Preserve one suspended job and epsh ownership of child waits.
- Keep completion, listing, and Git inspection in-process. Reuse epsh syntax information; editor inspection must not execute substitutions or introduce another parser/AST.
- The debugging-only `?` command is removed. Terminal integration owns output navigation/search/selection. The PTY relay remains for terminal handling and suspended jobs; filesystem quarantine is unrelated.
- Do not add `denv status`, `denv diff`, or other denv introspection. The nested-session finding below concerns existing state restoration.
- Preserve matched-workload performance evidence for interactive changes: prompt/completion latency, allocations, release size, and resource behavior. Never trade away execution, byte-preservation, or durability guarantees.
- Update the README contract and focused behavior tests when implementing changes. Keep native-terminal evidence separate from model proofs and deterministic PTY checks.

ISH-75's argument-aware feature remains deferred. Preserve the existing owner
priorities and won't-do decisions; ISH-89's priority is assumed from review
impact and likelihood, and its RFC awaits an owner decision.

## Open implementation tickets

### ISH-77 · P1 · Improve line editing and autosuggestion acceptance

- **Status:** In progress (2026-11-09).
- **Area:** Editor UX
- **Where:** `src/line.rs`, `src/editor.rs:424`, `src/editor.rs:513`, `src/editor.rs:585`, `src/editor/tests.rs`, `src/input.rs`, `src/render.rs`, `tests/pty.rs`, `README.md`
- **Delivered:** Ctrl+Right at the end of input accepts the next word of the history autosuggestion while Right still accepts the whole suggestion (`suggestion_word_accept_len`, editor key handling). Word movement is path-aware and stops at `/` separators. Bounded undo/redo (Ctrl+_ / Alt+R, 256-entry stacks) covers normal-mode typing/paste/Up-Down history recall and common edits; typing coalesces and navigation breaks groups. Input decoding recognizes Ctrl+`\`, `]`, `^`, and `_` bytes. Focused `line` and `editor` unit tests added; README documents bindings. Completion acceptance (unique, selected, or refiltered) and Ctrl+R acceptance each record one undo step and invalidate redo; previews and cancelled completions add none.
- **Remaining:** Undo/redo grouping for alias expansion boundaries, multiline edits, cancelled pickers, quoting/path cursor movement cases, combining/wide/narrow/wrapped PTY coverage, and warm-path allocation/memory verification.
- **Dependencies:** Undo/redo can proceed independently; share syntax-aware movement work with ISH-75/78.
- **Fix with:** `make-invalid-states-unrepresentable`
- **Effort:** M

### ISH-80 · P1 · Establish a terminal interoperability regression matrix

- **Status:** Deterministic PTY gaps addressed (2026-11-09); native/manual matrix remains a standing follow-up.
- **Area:** Tests / terminal correctness
- **Where:** `tests/pty.rs`, `tests/external_signals.rs`, `tests/login_shell.rs`, `docs/ptytest.md`, [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md), `.github/workflows/ci.yml`, terminal integration documentation
- **Delivered:** [docs/terminal-interop-matrix.md](docs/terminal-interop-matrix.md) separates deterministic PTY coverage from native/manual observations. Automated PTY coverage includes OSC 7, OSC 133 lifecycle (order/status, cancel/refusal, continuation, suspend/fg, redirection isolation), delegated `ish -c` non-emission, bounded resize storms, bounded repeated stop/resume, single-command `/dev/tty` input (`converged_command_reads_from_dev_tty`), alt-screen sequence relay passthrough with shell responsiveness (`alternate_screen_sequences_pass_through_relay`), raw OSC 52 clipboard emission for `copy-scrollback` (`osc52_copy_scrollback_emits_base64_session_log`), and SIGHUP-at-prompt exit/restore (`shell_sighup_at_prompt_exits_and_restores_terminal`).
- **Remaining:** Reproducible native/manual evidence for Ghostty, an independent terminal, SSH sessions, and tmux (prompt navigation/output boundaries, notification/completion behavior where supported), plus focused instrumentation/native runs for leaked descriptors/children, hung waits, lost input, terminal-mode restoration across native environments, and external child hangup/EOF edge cases. Unavailable native environments remain explicitly unverified, not passes.
- **Dependencies:** Acceptance gate for ISH-73/74 terminal behavior; retain as standing evidence, not a claim that PTY emulates real terminals completely.
- **Problem:** Native-terminal behavior remains unverified beyond the recorded deterministic PTY cases. The specific signal-during-execution defects found by this review have their own implementation tickets below.
- **Recommendation:** Complete the named native/manual observations and record failures separately from deterministic PTY evidence. Do not expand this into a new CI or generic smoke-test project.
- **Fix with:** `test-audit`
- **Effort:** M

### ISH-75 · P1 · Add argument-aware in-process completion

- **Status:** Deferred (potential blast radius).
- **Area:** Completion
- **Where:** `src/editor.rs:1429`, `src/editor.rs:1448`, `src/editor.rs:1469`, `src/complete.rs`, `src/builtin.rs`, `src/path.rs`, epsh lexer/parser interfaces, `docs/completion.md`, editor/integration/PTY tests, `benches/bench.rs`
- **Recommendation:** Complete ish builtin options and operands, common command subcommands/options, and Git branches, tags, and remotes. Start with a finite native set, using builtin metadata as the source of truth where possible. Derive the command and argument position from reusable epsh lexical/syntax information instead of expanding `shell_tokens`, `split_whitespace`, and ad hoc command-position heuristics. Add a bounded tolerant inspection API in epsh if incomplete input requires one; do not build a second parser or evaluate the line. Read Git metadata in-process with explicit worktree/common-dir, loose/packed-ref, invalidation, and resource-bound behavior.
- **Acceptance:** Tests cover incomplete quotes, escapes, assignments, redirections, command lists/pipelines, aliases, supported wrappers, mid-line insertion, option arguments, and `--`. Suggestions preserve quoting and operand bytes, remain aligned with dispatch restrictions, and never execute substitutions. Git fixtures cover worktrees and changed refs without stale suggestions. Preserve existing path/host matching and ordering contracts unless an intentional change is documented. Benchmark cold/warm completion and large candidate sets; no subprocess-per-keystroke behavior, plugin ecosystem, or general completion configuration language.
- **Deferral note:** Deferred for now due to potential blast radius across the epsh inspection surface, completion hot path, and in-process Git metadata behavior. Revisit as a focused, benchmark-gated change.
- **Dependencies:** Shared syntax inspection should also serve ISH-78 and syntax-aware editing in ISH-77.
- **Fix with:** `single-source-of-truth`
- **Effort:** M

### ISH-89 · P2 · Nested denv sessions lose project restoration provenance

- **Area:** Data
- **Where:** `src/denv.rs:143`, `src/denv.rs:227`, `src/denv.rs:306`, `src/denv.rs:322`.
- **Problem:** `init` replaces the inherited record ID, then the inherited `__DENV_STATE` can satisfy the refresh fast path without an active restoration record. A real-API probe approves `.env` changing a value from `baseline` to `project`, starts a child shell with the parent's exported environment, then leaves the project. The child retains `project` while clearing `__DENV_DIR`; the original shell restores `baseline`. The state/provenance mismatch is demonstrated; nested-shell inheritance semantics are not settled by the existing accepted RFCs.
- **Recommendation:** Adopt an explicit nested-session contract. Prefer independently owned restoration provenance for an inherited active layer, without reusing or deleting the parent's record. Preserve random session IDs, exact binding bytes/export state, and atomic store publication. Do not reopen denv introspection or history-privacy features.
- **Fix with:** `durable-state`
- **Effort:** M (days)
- **Design:** [Nested denv restoration](docs/rfc/ISH-89-nested-denv-restoration.md).

### ISH-45 · P3 · Native macOS privileged ownership acceptance remains incomplete

- **Status:** The accepted restrictive-umask boundary is implemented. Ordinary-user macOS workspace tests, `make test`, and strict workspace Clippy passed. Only privileged ownership acceptance remains. Re-checked: `clone_declines_inherited_read_denial_before_creation` and `clone_declines_before_creation_when_owner_read_is_absent` still take the root branch only when `geteuid` is 0, and `private_directory_prepares_foreign_owned_staging_directory` is still ignored.
- **Area:** Tests
- **Where:** `crates/ish-fs/tests/native_macos.rs:210`, `crates/ish-fs/tests/native_macos.rs:304`, `crates/ish-fs/tests/native_macos.rs:562` (`clone_declines_inherited_read_denial_before_creation`, `clone_declines_before_creation_when_owner_read_is_absent`, `private_directory_prepares_foreign_owned_staging_directory`), `docs/filesystem-testing.md`
- **Problem:** The foreign-owner clone branches and staging case require native root access to change ownership and drop credentials. The recorded ordinary-user passes do not establish those branches; this review did not run native macOS tests.
- **Recommendation:** Run the two clone-decline cases as root and the ignored foreign-owned staging case using the commands in the filesystem testing guide. Close this ticket only after those native branches pass. The implementation and support-boundary decisions are settled.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-78 · P3 · Add parser-aware highlighting and diagnostics

- **Status:** Planned (lower priority).
- **Area:** Editor feedback
- **Where:** `src/editor.rs`, `src/render.rs`, `src/dispatch.rs`, epsh lexer/parser interfaces, editor/PTY tests, `README.md`
- **Recommendation:** Extend command-existence coloring to shell words, quotes, operators, redirections, and unsupported/invalid syntax using the shared epsh inspection path. Distinguish incomplete input that legitimately needs continuation from a definite syntax/policy error. Validate commands at each command position rather than treating the entire line as one whitespace-delimited command. Keep the visual vocabulary fixed and small; no themes or configurable highlighter language.
- **Acceptance:** Highlighting and completion agree on spans and command positions for aliases, assignments, nested substitutions, multiline input, and supported operators. Inspection never evaluates substitutions, performs redirections, or mutates shell state; actual dispatch remains authoritative. Highlighted text cannot inject terminal controls. Snapshot/PTY coverage checks cursor geometry and Unicode/wrapping, and benchmarks quantify repaint/inspection cost.
- **Dependencies:** Reuse the tolerant lexical/syntax work from ISH-75 and integrate with ISH-77's edit model.
- **Fix with:** `single-source-of-truth`
- **Effort:** M

### ISH-79 · P3 · Make interactive pickers consistent

- **Status:** Planned (lower priority); does not include a captured-output browser.
- **Area:** Interactive UX / focused reuse
- **Where:** `src/editor.rs`, `src/mode.rs`, `src/render.rs`, `src/complete.rs`, history/directory-picker tests, `README.md`
- **Recommendation:** Make Ctrl+R history search, Tab completion, and Ctrl+Backspace directory selection a coherent family: consistent acceptance/cancellation, keyboard navigation, filtering/paste behavior, empty states, and resizing. Retain useful domain differences such as completion's grid and directory selection's navigation purpose. Share small layout/input helpers only where that removes duplication; do not introduce a generic TUI framework or change `fdu`'s independent browser contract gratuitously.
- **Acceptance:** A documented key/behavior matrix and focused tests show that cancellation restores the intended original input, previews do not execute commands, selection remains stable as results change, and resize/paste/multiline cases leave no stale rows or displaced cursor. Memory remains bounded and existing rendering caches/buffer reuse are preserved. Output search/selection stays terminal-owned under ISH-74 rather than adding a fourth picker backed by capture storage.
- **Dependencies:** Coordinate with ISH-75/77/78; preserve intentional existing paste/cancellation contracts unless explicitly revised.
- **Fix with:** `deduplicate`
- **Effort:** S–M

### ISH-82 · P3 · Give prereleases explicit version and revision identity

- **Status:** Planned (lower priority).
- **Area:** Release metadata
- **Where:** `Cargo.toml`, `src/main.rs`, `.github/workflows/release.yml`, `Makefile`, `README.md`, `docs/development.md`
- **Recommendation:** Define a lightweight development/prerelease version policy instead of leaving user-visible builds indefinitely identified only as `0.0.0`. Align package version, `ish -V`, release tags/names, and archive names, while retaining a source revision identifier for exact bug reports. Do not imply a stable API/behavior guarantee before one is intended, or turn this into a new release-management subsystem.
- **Acceptance:** A produced artifact can be traced unambiguously to its source revision and declared version. Local development builds and tagged prereleases report their identity consistently. Verify release metadata without weakening the existing verified-bytes, panic-diagnostic, static-Linux, or native-macOS release requirements.
- **Dependencies:** Coordinate published license metadata with the root MIT license; preserve ISH-42's release contract.
- **Fix with:** `single-source-of-truth`
- **Effort:** S

## Won't do

### ISH-76 · Selective history privacy and execution metadata

- **Status:** Won't do.
- **Reason:** The owner declined leading-space exclusion, selective cross-shell deletion, compaction/delete records, and versioned execution metadata. Preserve that decision. The current `~/.local/share/ish/history.log` is a checksummed binary journal, not editable line-oriented text; use `history` to inspect it and `history reset` to clear it.

### ISH-60 · Journaled retirement makes large flat EXDEV moves take minutes

- **Status:** Dropped.
- **Reason:** Parked candidate exists but timing proof requires quiet-host, interleaved baseline/candidate measurement (weeks-scale effort). Not worth pursuing now.
