# Execute the ish semantic foundations campaign

Status: executing. Owner granted implementation, local commits, the unsquashed
epsh import, and the described CI edits on 2026-10-17. No pushes or publication.

Build one Rust workspace containing ish and epsh, replace Perl conformance testing,
and establish an audited Lean model connected to production Rust behavior.
Complete semantic coverage before optimizing the implementation.

Deliver these results:

1. Import epsh with its history under `crates/epsh`, preserving its CLI, embedding
   API, MIT attribution, version, and independent packageability.
2. Replace the legacy runner with Rust tests that preserve all 175 retained case
   IDs, assertions, bytes, and outcome classifications.
3. Enforce ish's documented syntax policy through nested and dynamic execution.
   Give epsh arithmetic the owner-approved contract below.
4. Build a pinned Lean project, substantive proofs, an axiom audit, and a
   differential bridge to actual Rust entry points. Report model proofs and
   implementation correspondence separately.
5. Preserve interactive, process, release, and packaging behavior. Make benchmark
   execution independent of sibling repositories. Retain measured improvements
   only after deliverables 1–4 and their tests are complete.

## Charter

- Read the current working tree and applicable instructions before every slice.
  Ignore undo entirely. Never restore it or include concurrent owner changes.
- Settle technical choices inside the bounds below and record material calls.
  Stop only when every deliverable passes or every remaining item has a hard stop.
  A parked dependency prevents its dependents from starting.
- Hard stops are missing execution authority, a forbidden contract change,
  destructive or external effects, and an environment with no usable checker.
  Park only the affected item and continue independent work.
- On failure, obtain a minimal failing test and establish the cause before editing.
  After two refuted hypotheses, bisect the input or path. After three failed fixes
  or 45 minutes without new evidence, hand the reproduction to a stronger lane.
  The integrator resolves technical blockers. Park after a second bounded attempt
  only if an external prerequisite or forbidden boundary actually prevents work.
- Fix a baseline failure that blocks a deliverable, any focused warm check taking
  over 30 seconds, and any confirmed resource regression over 2× baseline.
  Investigate commands exceeding five minutes before repeating them.
  Keep these fixes separate from feature work. Record unrelated findings and stop.
- Before parity, measure resource costs without making ordinary performance
  budgets gate feature integration. The 2× limit is the exception. Final performance
  gates still block calling the campaign complete.
- Do not run formatters or automatic fix targets. Do not change the Rust nightly.
  Do not push, publish, tag, install the shell, or modify account or system state.
- Use `.work/wt/<lane>` for worktrees and `.work/tmp` for scratch. Before the first
  write, confirm `.work/` is ignored through `.git/info/exclude`. Set `TMPDIR` to
  an absolute `.work/tmp` path in every lane and subprocess environment.
- Keep this document as the sole campaign plan and short execution log. Keep only
  the corpus comparison and verification coverage records needed to detect loss.
  Do not create another campaign document or inventories of repository provenance.
- Local commits require an explicit execution grant. When granted, use Conventional
  Commits with a lowercase imperative subject of at most 72 characters. Put item
  IDs and reasoning in the body. Contract changes use `!` or a breaking-change footer.
- Code comments follow AGENTS.md. Never cite this plan from code.

### Run the lanes

Use the `orchestrate` skill and its campaign reference for lane briefs, integration,
verification worktrees, model selection, and worktree lifecycle. This plan supplies
ownership and limits rather than replacing those rules.

The integrator owns shared manifests, Cargo.lock, Makefile, toolchain pins, workflow
files, README, this plan, and shared formal imports. Lanes propose edits to those
files in their reports. One lane owns each production file at a time. The integrator
reviews, integrates, and verifies, and writes no feature code while lanes run.

Maintain four lanes when four disjoint slices are ready. Expand to six during
formalization. The floor is the smaller of ready ownership sets and the resource
ceiling. Budget 3 GiB RAM and two CPU slots per lane, reserve 8 GiB for the host,
and cap at eight lanes and two simultaneous compiler-heavy checks. Reduce width
only for measured memory, active swap growth, CPU, or disk pressure. Measure peak
RSS on the first slices and raise the per-lane allowance if necessary.

A lane that stops is resumed or replaced in the same turn while ready work exists.
Start each later slice as soon as its own prerequisites land. Integrate completed
slices while the next tranche runs. Verify independent case groups in parallel
from separate revision-fixed worktrees, never two builds against a changing tree.
If verification queues dominate, assign a loop-speed lane before increasing width.
Every wait names the next scheduling action. Revisit technical blockers each round.

Before removal, stop the lane, review its full status including ignored files,
integrate or preserve its work, and pass integrated checks. Never force-remove a
worktree or branch. Follow the orchestrate closeout guard when available. Retain a
clean worktree only with its path and reason in the handoff and guard allowance.
Without commit authority, preserve uncommitted work and report its path.

## Decisions

### Owner-decided

- 2026-10-17: Lean is already set up. Use the installed toolchain. Preflight verified
  Lean 4.34.1 and Lake. Create the repository project, but do not reinstall Lean.
- 2026-10-17: upgrade this project to the installed Lean 4.35.0 and run
  `lake check --paranoid` over all model families. Keep the Rust nightly pin.
- 2026-10-17: Arithmetic uses wrapping i64 addition, subtraction, multiplication,
  and negation. Division by zero and signed division overflow return arithmetic
  errors. Shift counts outside 0–63 return arithmetic errors. Apply the same rules
  to the existing compound assignments. This removes profile-dependent panics and
  is a contract change.
- Preserve the supplied draft's product scope: interactive-only ish; independently
  usable non-interactive epsh; no undo work; semantic verification before optimization.

### Prescribed

- Complete the arithmetic decision consistently: remainder by zero or MIN % -1
  returns an arithmetic error. Left shift keeps the low 64 bits; right shift is
  arithmetic. Preserve the existing grammar; do not add increment/decrement.
- Preserve epsh's existing public signatures and CLI. For ish enforcement, an
  additive, opt-in epsh policy hook is permitted only if existing embeddings keep
  their behavior. Check expanded command targets before builtin/function dispatch,
  including `command` indirection and nested evaluation. An external-handler-only
  filter is insufficient because epsh resolves builtins first.
- Dynamic targets `eval`, `source`, and `.` are refused with ish status 2. An
  expanded InShell builtin cannot enter the stateless evaluator. Preserve ordinary
  dynamically named commands. Refuse statically forbidden syntax before any effect.
  For dynamically discovered targets, require refusal before that target's effects;
  do not promise rollback of earlier commands or expansion effects.
- Preserve normal epsh access to eval, source, functions, and compound execution.
  Keep denv evaluation separate from the interactive input policy. Do not turn ish
  into a sandbox against external programs explicitly invoked by the user.
- Use one workspace, root lockfile, and root nightly-2026-09-15 pin. Import tracked
  epsh contents with unsquashed local history. Leave the sibling checkout untouched.
  Do not import its nested .git. Preserve upstream legal notices with the corpus.
- Keep all 175 legacy IDs. A known failure remains an explicit executed expected
  failure, and an unexpected pass is reported. Category exclusions remain visible
  with their host predicate. A baseline mismatch is never made green by skipping it.
- Preserve the raw corpus during migration. Parse bytes, not lossy UTF-8. Support
  all observed directives, including `need-pass` and `info-pre`. Remove the Perl
  runner only after old and new outcome parity on the same binary.
- Pin `formal/lean-toolchain` to `leanprover/lean4:v4.35.0`, as authorized by the
  owner's toolchain update. Use Lean's standard
  library. Mathlib and new translation toolchains are outside the default grant.
  Investigate Aeneas using already installed tools for at most one working day.
  Keep a working refinement pilot only if compatible with the existing pins and
  requiring no production architecture distortion. Otherwise document the limitation.
- Use a development-only, version-1 JSON-lines bridge with tagged outcomes and
  byte arrays of integers 0–255. Reject unknown fields, versions, malformed bytes,
  and duplicate case IDs. No production dependency or persistent user format changes.
  Use jaq for JSON inspection. The interchange writer has one Rust owner.
- Models use explicit success, semantic error, unsupported-domain, and fuel-exhausted
  outcomes. Fuel exhaustion is never success or a termination proof. Model external
  effects with input observations and ordered traces. Compare real process behavior
  separately; abstract descriptor proofs do not establish kernel descriptor safety.
- New dependencies are development-only by default. Prefer existing tempfile and a
  small JSON implementation; use a small regex dependency only if observed patterns
  require it. Inspect maintenance, license, transitives, and available local code
  before selecting. No general test framework or arbitrary expression interpreter.
  Reject production dependency additions unless already required for the policy hook.
- Capture baseline clean-build CPU time, warm-check time, peak RSS, allocations,
  and release size using the same target and profile. Report changes in each.
  Keep the 2× egregious-regression ceiling. Other measurements are advisory.
  An optional optimization must improve its representative
  workload beyond observed spread without worsening the other measured costs
  beyond their spread. Reject the optimization otherwise. Development dependencies
  must not enter production artifacts and remain within the 2× build/RSS ceiling.
  Report their build-time and binary-size impact. Report formal build costs
  separately because no formal baseline exists.
- CI edits are a separate, explicitly authorized work item. The plan records requested
  CI integration, but the executor must not infer permission from this document.
  Formatting checks and formatter runs are excluded.

### Delegated within bounds

- Choose Rust harness organization, Lean module internals, case generator, and
  fixture storage inside the named files. Prefer the smaller design when both
  satisfy every acceptance check. Record the choice and its reason.
- Extract pure helpers only when a real test or proof needs them. Do not expose
  private storage or add production configuration for the bridge.
- Select at most three measured optimization candidates at P4. Keep none if gains
  are within noise. Investigating and rejecting all candidates can satisfy P4.
- New public breaks, changed history/config/denv formats, changes to accepted syntax
  beyond enforcing the README, new runtime dependencies outside the bound above,
  and platform drops are forbidden. Park those proposals with a concrete reason.

## Baseline and fast loop

Observed on 2026-10-17:

| Fact | Result |
| --- | --- |
| ish source | `7e26f07fda8a1f7891daa472b0a9389d78890adf`; only staged addition was lean.md |
| sibling epsh source | clean at `6b47acb0577be1ea1eac5505abf66a498cb1614f` |
| Dependency | ish uses registry epsh 0.0.9; no workspace or formal project yet |
| Source parity | all sibling epsh src files match cached registry 0.0.9 byte-for-byte; native test files match, stress scripts are excluded from package |
| Toolchains | both Rust pins are nightly-2026-09-15; Lean 4.34.1; Lake 5.0.0-src |
| Host | Linux, 32 logical CPUs, about 60 GiB RAM; remeasure headroom before scheduling |
| Focused dispatch tests | seven passed, 132 filtered out; 0.17 s wall, 0.02 s user, 0.02 s system |
| Lean foundation probe | byte-list append-length theorem checked through stdin; 0.73 s wall, 0.41 s user, 0.31 s system; axioms `[propext]` |
| Runtime policy probe | rebuilt debug ish and exercised a private PTY; expanded eval and command eval both executed |
| Legacy corpus | 175 unique IDs; invalid UTF-8 present in glob-range-3 |
| Benchmarks | rustybench 0.3 already used; make bench defaults to sibling rustybench CLI |

Timings above are single warm diagnostic observations, not performance gates.
The Lean probe establishes a working checker, not shell semantics.
The policy probe ran `x=eval; $x 'PREFLIGHT_DYNAMIC=1'` and
`command eval 'PREFLIGHT_INDIRECT=1'`, then observed both assignments in ish.
Keep a regression for each bypass in P1.

Full Rust, legacy Perl, packaging, release, and platform baselines were not run in
preflight. No all-green baseline is claimed. Arithmetic overflow/shift instability
is established by source inspection; produce minimal failing tests in P1.

Every shell command uses `/home/josh/.cargo/bin` on PATH when this environment
needs it. Rust compilation must retain the configured kache wrapper and musl mold
linker. Reuse artifacts, never cargo clean during iteration.

Use two verification speeds:

- Per edit: `cargo test -p ish --lib dispatch::tests`, or the named epsh test,
  one conformance ID, one Lean module, or one bridge family. No release builds.
- Phase gates: the touched crate's unit and integration targets, full affected
  corpus, and affected Lean modules. Run the workspace Rust suite at P1 and close
  because workspace relocation and final integration affect both crates. Run the
  complete bridge at P2 and close because its families share the wire format.

Time the first real command for each new loop. A warm single-case Rust check must
finish within 10 seconds, a single bridge case within 5 seconds, and a focused
Lean module within 15 seconds. Over 30 seconds requires a loop fix first. Split
selectors or modules and reuse artifacts; never weaken the accepted behavior.
Cold compilation is measured separately and does not masquerade as warm-loop cost.

Time-based performance gates run only after `perf-gate-ready` exits 0 and with
`perf-gate-ready -1` immediately afterward. Discard overlapped runs. If the host
stays busy, report "perf gate deferred: host busy", continue correctness work,
and leave performance acceptance open. Always run memory and size checks.
Pause compiler-heavy lanes for measurements. Interleave at least five baseline
and candidate runs with identical fixtures and release settings. Prefer CPU time,
report minimum plus median and range, and fail only beyond the observed spread.
The checked-in macOS benchmark JSON is not a Linux performance baseline.

## Sequence and ownership

Sizes are lane sessions: S is up to half a session, M is one, L is several resumed
sessions. They bound handoff size, not proof difficulty. All items include their
focused tests and documentation. Proposed paths below become real during execution.

### P0: establish the ground

Maximum width: four read-only lanes, then one integration writer.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| G0 | M | execution grant | integrator: this plan, scratch setup; scouts read both trees | current instructions and owner changes inspected; source comparison, corpus inventory, failures, and narrow timings recorded |
| G1 | M | G0 | integrator: workspace/import/shared build files | unsquashed local import prepared and reviewed; no sibling writes; one lockfile/pin; path epsh resolves locally; focused ish dispatch and epsh api_stability checks pass |

G0 runs each existing native suite once per repository, with epsh artifacts directed
inside ish's `.work/`. Run the old Perl corpus once if Perl is available. Capture
case IDs and pass/fail/excluded/ignored-failure results. If Perl is unavailable,
implement a byte-faithful reader first and retain the old runner until an equivalent
recorded-output oracle exists. Do not invent baseline outcomes.

Import only from a clean tracked epsh source. If the owner has new dirty epsh edits,
import its committed tree, leave those edits untouched, and park integrating them.
For dirty ish files, hand off disjoint patches without staging the owner's content.
Do not reset, stash, clean, amend, rewrite existing history, or force-checkout.

### P1: finish behavior and native test parity

Maximum width: four lanes after G1. Formal specification work in F0 can overlap as
soon as its interface is fixed. The integrator serializes shared-file changes.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| T1 | L | G1, old runner baseline | crates/epsh/tests/conformance*, retained corpus, legacy runner removal | all 175 IDs and assertion outcomes match on one binary; focused ID/category selectors; malformed fixture and timeout-cleanup tests fail when mutated |
| E1 | L | G1 | crates/epsh/src plus existing native tests, excluding T1 files | arithmetic contract and opt-in dispatch enforcement hook; API/embedding/byte tests; real tests for every changed boundary |
| I1 | L | E1 policy-hook signature only; G1 for preparatory tests | src/dispatch.rs, src/main.rs, src/builtin.rs, tests/pty.rs, tests/integration.rs | literal/nested/dynamic/indirect policy refusals, positive dynamic commands, whole-line builtin routing, status 2, no forbidden target effects |
| F0 | M | G1; stable owner decisions | formal project skeleton, Bridge schema/Protocol module | pinned Lake project; typed byte/state/outcome/trace vocabulary; golden wire examples accepted by Rust and Lean; no runtime Lean dependency |

T1 first characterizes runner behavior for raw bytes, tab blocks, trailing newline
chomping, script plus stdin/arguments, env setup/removal, file/dir/symlink setup,
permissions, categories, time limits, exit predicates, and all 12 stderr patterns.
Implement only observed exit expressions: integer, `e == 0`, and `e != 0`.
Translate observed regex features explicitly, with old/new predicate equivalence
cases. Unsupported syntax is a parse error. `need-pass: no` executes and classifies
an ignored failure; `info-pre` is informational. Never convert either into a skip.

Confine fixture paths to per-case `.work/tmp` directories. Reject absolute paths,
parent traversal, and symlink escape during setup. Use an isolated environment and
working directory, bounded output capture of 8 MiB per stream, and the case time
limit or a default 10 seconds. Output overflow fails the case. The runner owns
children and process groups, drains both streams, terminates on timeout/cancellation,
and reaps before returning. Prove cleanup with a process-tree regression.

E1 owns all epsh production changes to avoid arithmetic/evaluator conflicts.
Test MAX+1, MIN-1, multiplication/negation overflow, MIN/-1, MIN%-1, zero divisors,
shift counts -1/0/63/64, precedence, lazy operators, and assignment side effects.
Failing arithmetic operations leave their assignment destination unchanged and
propagate the existing arithmetic-error status path. Earlier effects remain.

At P1's end, run `cargo test --workspace --lib --tests`, the full native corpus,
and focused PTY policy/descriptor/job tests. Document intentional arithmetic and
policy differences beside their tests. All completeness tests must exist and pass
before claiming P1 complete. Keep baseline expected failures explicit.

### P2: prove and connect the semantics

Maximum width: six lanes, one owner for each F1–F6 module family. F0 owns the protocol
until its interface lands. The integrator owns shared imports and test registration.
No production optimization is permitted during P2.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| F1 | L | F0, I1/E1 completed for correspondence | formal/IshSemantics/Policy*, family policy fixtures | policy determinism, structural exclusion over every nested AST position, runtime target exclusion under the installed policy; real plan and runtime bridge cases |
| F2 | L | F0; E1 semantics for arithmetic integration | formal/IshSemantics/Expansion*, family expansion fixtures | quoted/literal preservation, unset/empty, IFS, quoted at-parameters, concatenation/ordering, glob quoting/unmatched behavior, comsub newline integration |
| F3 | L | F0, E1 arithmetic contract | formal/IshSemantics/Arithmetic*, family arithmetic fixtures | bounded i64 evaluation, precedence/associativity, lazy evaluation, assignment effects, overflow/error boundaries; real Rust arithmetic execution |
| F4 | L | F0, E1 evaluator behavior | formal/IshSemantics/Evaluation*, family evaluation fixtures | sequential/AND/OR/negation/pipeline status and trace laws, errexit tested contexts, assignment scope and state isolation |
| F5 | L | F0, E1 byte/environment behavior | formal/IshSemantics/Effects*, family effects fixtures | environment precedence/noninterference, byte round trips with NUL preconditions, per-shell cwd isolation, abstract descriptor-state restoration and separate process tests |
| F6 | L | F0 | formal/IshSemantics/Interactive*, family interactive fixtures | LineBuffer insert/delete/kill/yank preserve cursor range and UTF-8 boundaries; real production transitions; PTY behavior remains tested separately |
| B1 | L | F0 protocol; each family as delivered | Rust bridge driver, bridge orchestration/tests, formal executable main, docs/verification/coverage.md | each family compares actual Rust results against Lean; deterministic seeds, shrinking, permanent minimized regressions, mutation negative controls |

B1 can be the seventh lane only when resources allow; otherwise keep its skeleton
in F0 and assign a freed family lane immediately. Never postpone bridge integration
until all proofs finish. Start each family's bridge as its first definitions land.

Model Program.commands and the actual Command/WordPart/Redir vocabulary, preserving
byte literals and ordered redirections. Give out-of-domain epsh control flow an
explicit unsupported result. Cover every ish-accepted AST constructor. Model
termination-sensitive evaluation with fuel and explicit exhaustion. Abstract kernel,
filesystem, terminal, signal, and scheduling behavior with stated assumptions.
`exec` has permanent redirections and is outside temporary-restoration theorems.
F5 models a descriptor map, owned saved copies, and partial setup rollback. Prove
that normal completion or setup failure restores the original map when saved
copies are fresh and restoration operations succeed. Represent restoration failure
as an error rather than assuming it away. Real process tests separately observe
pre/post descriptor mappings, output destinations, and leaked descriptors on failure.

Each family needs independently stated semantic laws plus counterexamples for
weakened laws. Include quoted zero-versus-one fields, invalid bytes, empty state,
nested substitutions, arithmetic boundaries, failed redirections, and invalid syntax.
The bridge receives source plus explicit state. Its Rust side invokes the real
parser, planner, evaluator, and LineBuffer methods. It must not duplicate their
algorithms. A parser-to-model AST adapter is tested against known expected trees;
it cannot independently certify parser correctness. Compare statuses, raw outputs,
state, environment, invocation and descriptor traces where observable. Normalize
only process IDs and sandbox root paths. Unsupported cases fail selected-family
runs rather than disappearing from the totals.

Start with fixed fixtures per behavior. Add a fixed-seed generator with depth at
most 8, 64 commands, 256-byte words, and 1,000 cases per family at its phase gate.
Keep one-ID and family selectors. Set Lean execution fuel explicitly per case.
A negative control changes one result in every family and must produce a semantic
mismatch. Also mutate a theorem dependency to `sorry` in a disposable worktree and
show the audit rejects it.

Generate the theorem audit from the exported acceptance theorem list. Require all
listed declarations to exist, inspect transitive `#print axioms`, and permit only
Lean's standard `propext`, `Quot.sound`, and `Classical.choice`. Reject sorryAx,
custom axioms, admits, and missing declarations. Do not count theorem quantity as
proof quality. Coverage rows identify domain, theorem, Rust entry point, test,
assumptions, and whether correspondence is proved, tested, or unestablished.
Validate those references in `make verify`.

A difficult proof is an unfinished deliverable. Narrowing its required domain needs
owner authority and is otherwise parked. The Aeneas pilot may be unavailable without
blocking independent model proofs; the final report must state its actual result.

### P3: harden and document

Maximum width: four lanes grouped by disjoint production owners and documentation.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| H1 | L | T1, E1, I1, F1–F6, B1 | resume each affected production/proof owner | failure, signal, cancellation, descriptor, non-UTF-8, and policy findings reproduced and fixed; all family checks pass |
| D1 | M | delivered interfaces | root/crate docs and AGENTS, coordinated by integrator | live architecture/testing/package/formal instructions; no Perl dependency; assumptions and contract changes disclosed |
| C1 | M | H1, explicit CI edit grant | integrator: workflow files only | existing platform/release coverage retained; both crates, corpus, Lean audit, bridge wired into requested checks; production musl image remains Lean-free |

Expose `make test-epsh`, `make conformance`, `make verify`, and `make check` through
simple commands. `make verify` checks Lean, axiom/coverage audits, and the bridge.
`make check` adds read-only Clippy and Rust tests. Never call existing fix/lint
recipes that invoke formatters or automatic edits.

Do not remove externally exposed helper binaries merely because they look like
fixtures. Preserve package targets during consolidation and record any unwanted
packaging separately. Check `cargo package -p epsh` and
`cargo publish -p epsh --dry-run` from a clean verification worktree. Never run the
old epsh make publish recipe. Keep MIT licensing scoped to epsh.

### P4: optimize and polish

Maximum width: four disjoint production owners. Begin only after P1–P3 correctness
and proof acceptance passes. A parked mandatory proof blocks this phase.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| O1 | M | P3 gate | integrator: Makefile benchmark entry; benches/bench.rs only for required selectors | make bench uses the published rustybench 0.3 CLI with installation under .work/ if needed, no sibling checkout; existing workload names and useful allocation measurements retained |
| O2 | L | O1, measured profile | at most three disjoint epsh/ish hot-path slices | representative gains or explicit rejected candidates; focused Rust/Lean/bridge/PTY checks; final budgets pass |

Use the existing startup, enter-to-prompt, render, completion, history, directory,
and filesystem workloads. Profile separately from timing. Prioritize removing
repeated parsing, allocations, copying, conversion, lookups, and intermediate fields.
Do not require an arbitrary speedup or a rewrite. Reject speculative caches,
parallelism, abstractions, and representation changes without measurable benefit.
Document cache authority/invalidation if a measured candidate actually needs one.
Run all family parity checks before retaining a semantically relevant optimization.

### P5: close

Maximum width: read-only review lanes plus independent verification targets.

| Item | Size | Prerequisites | Ownership | Acceptance |
| --- | --- | --- | --- | --- |
| Z1 | M | P4, or explicitly parked performance gate | integrator and scoped review lanes | dead migration paths deleted with caller evidence; ultrareview completed; P1/P2 findings fixed; final checks/report and worktree cleanup |

Run ultrareview at close and fix its P1/P2 findings within the execution grant.
Do not create new CI scope from review findings. A forbidden contract change remains
parked and blocks the relevant acceptance. Delete legacy runner/filter/check.t only
when consumers and regenerated input are no longer live; preserve required license
notices. Remove duplicate epsh lockfile/toolchain/release machinery during import,
but touch root workflows only under C1's grant.

## Acceptance

These are final checks, not lane iteration commands. Tell the owner before expensive
runs. A missing platform, tool, or idle host yields an explicit incomplete item.

| Deliverable | Executable proof |
| --- | --- |
| Workspace | cargo metadata/tree show one local epsh and one workspace lock; `cargo test --workspace --lib --tests` |
| epsh embedding | `cargo test -p epsh --test api_stability`; `cargo test -p epsh --test embedding`; existing integration/byte tests |
| Native corpus | `make conformance`; exact 175-ID comparison and old/new outcomes; runner negative controls and no live Perl consumers |
| ish policy and terminal behavior | `cargo test -p ish --lib dispatch::tests`; `cargo test -p ish --test integration`; `cargo test -p ish --test pty`; history_import target |
| Formal semantics | `make verify`; all F1–F6 laws, axiom audit, coverage validation, 1,000 seeded cases per family, mutation controls |
| Packaging | `cargo package -p epsh`; `cargo publish -p epsh --dry-run`; inspect the package without installing/publishing |
| Release integrity | final-only static/dynamic release builds and existing verify-release targets on supported hosts, with readiness checks for time gates; no unintended epsh fixture artifacts in ish releases |
| Platforms | native macOS Apple silicon, Linux x86_64 musl and Linux ARM64 musl Rust/PTY checks; unavailable hosts remain unverified |
| Resources | idle-host baseline/candidate measurements disclose costs and meet the 2× ceiling; optional optimizations satisfy the retention rule; memory and binary-size checks always run |
| Requested CI wiring | review C1 diff and validate local commands/config; remote CI status is unverified without an authorized remote run |
| Close | read-only Clippy, no proof holes, no lost cases, no active lanes, reviewed worktrees, final report |

The campaign ends with one of two truthful outcomes: complete with every acceptance
row satisfied, or incomplete with every remaining row parked behind a named hard
stop. Never call a deferred perf gate, unavailable platform, or unproved obligation
complete. Do not wait for owner approval to finish a report.

## Execution boundary

The owner authorized execution on 2026-10-17, including repository implementation,
local Conventional Commits, the unsquashed local epsh import, and the described CI
edits. The owner requested Luna high for import scaffolding. No plan text supplies
permission beyond that grant. Local build/test/package dry-runs and
isolated experiments are reversible. Retain these exclusions even after launch:

- No pushes, remote workflow dispatch, tags, actual package publication, system/user installations,
  login-shell changes, credentials, sibling writes, force operations, or history rewrites.
- No Rust nightly change, undo work, platform drops, history/config/denv format changes,
  new shell language features, scripts in ish, or production Lean runtime.
- No arbitrary API migration. The additive opt-in hook, arithmetic behavior, policy
  refusals, development wire schema, and build-command changes must be listed in
  the final report with tests and docs. Other contract changes are parked.

## Non-goals

Do not prove the kernel, terminals, process scheduler, or all of POSIX. Do not add an
ish AST, parser, general configuration language, plugin framework, background jobs,
control flow, functions, source, or eval. Do not formally model undo. Keep epsh's
broader grammar and independent CLI. Do not restructure the test suite merely to
increase coverage counts or replace the existing benchmark framework.

## Reference map

Read these live sources before touching their areas:

| Area | Current source and contract |
| --- | --- |
| ish acceptance/routing | [README](README.md), [architecture](docs/architecture.md), src/dispatch.rs, src/builtin.rs, src/main.rs |
| Interactive transitions | src/line.rs, src/input.rs, src/render.rs; [completion](docs/completion.md), [history](docs/history-search.md) |
| Terminal/process behavior | src/job.rs, src/sys.rs, src/signal.rs; [PTY tests](docs/ptytest.md) |
| Alias and denv boundary | src/alias.rs, src/denv.rs, src/config.rs; keep existing config language |
| epsh source before import | ../epsh/AGENTS.md, README.md, Cargo.toml, Makefile, docs/architecture.md, docs/testing.md, docs/byte-preservation.md, docs/process-model.md |
| epsh semantics after import | crates/epsh/src/{ast,lexer,parser,expand,arith,glob,eval,var,shell_bytes,encoding,redirect,sys,builtins}.rs |
| Existing regression coverage | tests/{integration,pty,history_import}.rs; epsh tests/{api_stability,embedding,integration}.rs |
| Legacy runner | sibling check.pl, check-epsh.t, filter-tests.pl, check.t; moved beneath crates/epsh during import |
| Resource workloads | benches/bench.rs, benches/baseline.json, root Makefile |

The retained corpus also contains descriptions, script setup, arguments, environment
setup, file setup, exact stderr, stderr patterns, exit predicates, category predicates,
and time limits. Its special glob-range-3 fixture is raw bytes. Parse every observed directive; reject unknown directives.

The AST is recursive inside epsh; ish uses it without owning another AST.
ShellBytes preserves arbitrary Unix bytes; shell syntax and variable names remain
text. The internal invalid-byte compatibility encoding is not the interchange format.
Per-Shell cwd must not mutate the embedding process cwd. Prefix assignments override
exports, which override inherited environment. Preserve narrow libc fork paths where
children continue Rust evaluation. Temporary redirections restore descriptors;
exec redirections are permanent. Cancellation and custom-handler child ownership
must remain explicit. Do not claim an external handler intercepts epsh builtins.

## Decision log

- 2026-10-17: split E1 into disjoint arithmetic and evaluator/policy owners; preserve import ancestry with local merge commits rather than rebasing upstream history.
- 2026-10-17: reuse locked regex-lite 0.1.9 for the 12 conformance patterns; MIT/Apache, no new transitive packages or production link dependency.
- 2026-10-17: add the owned, read-only `Variables::bindings_bytes` semantic snapshot rather than infer writes from arithmetic syntax. This preserves existing signatures and keeps private storage, flags, and caches private while exposing dynamically created values to correspondence checks.
- 2026-10-17: compare clean compiler builds with per-command `KACHE_DISABLED=1`, retaining the configured wrapper and Mold. Warm-cache restoration is a separate diagnostic, not evidence of compiler speed. The original baseline checkout has a verification-only empty workspace declaration because its nested location otherwise discovers the new parent workspace.

- 2026-10-17: overlap G1 import and F0 foundation with G0 baselines against fixed source revisions; neither changes baseline source.

Append one line per material executor judgment, with the call and its reason.

## Campaign log

- 2026-10-17 G0: ish library 139 passed; integration process SIGSEGV narrowed to concurrent libc identity lookup in directory listing. epsh library 146 passed/1 hardcoded grep-path failure; API 16 passed; embedding 43 passed; integration 223 passed/3 failures (closed-FD EBADF panic and two invalid-UTF8 test decodes). Legacy corpus 168 passed/7 visible category exclusions, all 175 IDs classified.
- 2026-10-17 G1: imported unsquashed epsh history in 148639e (upstream 6b47acb is ancestor), workspace 707b307. Integrated dispatch7/API16 checks pass. Single root lock/toolchain; sibling clean.
- 2026-10-17 F0: strict v1 byte/state/trace protocol and vocabulary integrated; Lean protocol goldens pass. Semantic errors retain failure state/trace for differential effect checks.
- 2026-10-17 P1 integration: native epsh library/API/embedding/integration/byte/expansion/descriptor targets and all 175 corpus classifications pass. ish library/history-import/integration pass; six PTY synchronization failures remain under investigation. Fixed baseline identity-lookup races, closed-descriptor handling, arithmetic boundaries, expanded-target policy, quote expansion, and byte-codec collisions.
- 2026-10-17 P2 integration: all six model families and transitive axiom/coverage audit pass; integrated `make verify` compares 1,000 generated requests per family and detects all negative controls. Adversarial review found unsupported-outcome accounting and expansion adapter byte gaps; these remain open, so the comparison gate is not yet accepted. Aeneas/Charon are unavailable; no Rust refinement proof is claimed.
- 2026-10-17 P3 integration: native test and verification commands, documentation, and authorized CI wiring landed. Remote CI, macOS Apple silicon, and Linux ARM64 remain unavailable locally. Final review, Clippy cleanup, deferred-heredoc input ownership, performance, packaging, and release checks remain in progress.
- 2026-10-17 P1–P3 acceptance: integrated workspace suite passes, including 134 PTYs; strict `make verify` passes 1,000 supported cases per family, all controls, protocol checks, and retained regressions. Axiom/coverage audit validates 49 theorem links; workspace all-target Clippy passes. Fixed all seven adversarial-review findings and the deferred-input lifetime/collision findings. `cargo package -p epsh --locked` and publish dry-run pass; final resource/release and close checks remain.
- 2026-10-17 P4: benchmark commands now bootstrap published rustybench 0.3.0 under `.work/tools`, preserve overrides, and handle spaced repository paths. No sibling checkout or production dependency is required. Clean-build pairs and all existing runtime/allocation workloads are being measured on Linux musl; no optional optimization has been retained.
- 2026-10-17 P4 measurement: five idle-host interleaved clean static-build pairs pass the 2× ceiling: baseline/candidate median CPU 51.87/53.58 s, peak RSS 1,242,408/1,243,092 KB, binary 1,098,088/1,157,736 bytes. All 14 runtime/allocation workloads pass five paired samples; timing ranges overlap, so no optional optimization is retained. Five fresh-target developer checks plus immediate warm repeats also pass: cold median CPU 32.14/35.40 s and RSS 402,364/403,964 KB; warm CPU 0.03/0.04 s. These are total developer costs, not isolated dependency costs. Candidate-only dev dependency serde_json 1.0.154 is MIT/Apache-2.0 and absent from production edges; regex-lite was already a benchmark transitive dependency. Measurements precede final review fixes and require final-source confirmation.
- 2026-10-17 Z1 review: reproduced multiline UTF-8 corruption, unquoted history paths, cross-paired completion paths, stale denv discovery/trust, lost denv restoration on errors, and descendant-held denv pipes escaping deadlines. Scoped fix lanes are active. Verification review also reproduced a conformance harness dependency on ambient TMPDIR and identified sequential-trace and unexported-variable adapter blind spots. Final acceptance is reopened until integrated fixes and checks pass.
- 2026-10-17 acceleration: widened ownership to eight lanes, including the remaining interactive defects, literal U+FFFE preservation, process/descriptor lifecycle fixes, corresponding expansion-model updates, and a compiler-queue owner. Integrated command/completion and verification fixes. Cold Lean build at 20f1320 passes all 29 jobs and 49 theorem references: 80.26 CPU s, 16.80 wall s, 1,701,152 KB peak RSS, with both readiness gates clear; final model edits still require confirmation. No push or publication.
- 2026-10-17 Z1 local correctness: all confirmed review findings are fixed and integrated. Final workspace tests pass, including 140 PTYs; all-target Clippy passes. Native conformance retains 175 cases: 168 pass and seven explicit exclusions. Lean 4.35.0 `make verify` passes all 6,000 supported cases, mutation controls, protocol checks, and 52 theorem links. Package verification, publication dry-run, a separate epsh consumer, and the final dynamic release's ELF checks and 140 PTYs pass. Final resource measurements, static release checks, and the all-kernel check remain open; native macOS/ARM64 and remote CI remain unavailable.
- 2026-10-17 Lean 4.35 acceptance at 353b215: narrowed blanket Lean imports without changing models, proofs, or fixtures; integrated `make verify` passes again. `lake check --paranoid` exits 0: hardened Lean, lean4lean, nanoda, con-leche, con-ron, and default Lean all accept the complete default library (95,742 declarations reported by lean4lean). Final axioms are exactly propext, Quot.sound, and Classical.choice. Nanoda uses its bundled four-worker configuration; the obsolete broad-import export had overflowed its worker stack. This host lacks `/run/user`, so a scoped COMPARATOR_BWRAP shim creates a private sandbox `/run` before its existing `/run/user` mount; no sandbox restriction or host directory was changed. The shim is unnecessary when the host has that parent or the runner creates it.

Append one line per phase end with commands, outcomes, contract changes, and remaining
hard stops. Baseline IDs and theorem coverage belong in their tested artifacts.

## Closeout report

Status: incomplete. All local acceptance checks pass. Native macOS Apple silicon
and Linux ARM64 hosts are unavailable; remote CI was not authorized.

The unsquashed epsh import and one-workspace conversion are integrated. The Rust
conformance corpus retains all 175 IDs: 168 pass and seven remain explicit,
category exclusions (`shell:legacy-no`, `shell:int32`, `shell:int64`, and
`shell:intu`). Lean 4.35 checks six semantic families: policy, expansion,
arithmetic, evaluation, effects, and interactive behavior. The bridge compares
1,000 generated cases per family with production Rust entry points, detects each
family's mutation control, and validates 52 theorem links. The model uses finite
fuel and supplied observations for external behavior. Aeneas and Charon are
unavailable, so no Rust refinement proof is claimed.

The final-source workspace tests pass 952 tests across 27 runners, including 140
PTY tests. All-target Clippy, the semantic bridge, epsh package and publish
dry-run, and a separate epsh consumer pass. After the final denv change at
088caa8, its focused suite passes 25 tests. Static and dynamic
`make verify-release` checks pass, including release ELF checks and 140 PTY tests
for each artifact. The static binary is 1,170,120 bytes, 72,032 bytes (6.56%)
above the 1,098,088-byte baseline; the dynamic binary is 1,111,296 bytes.
Development dependencies are absent from both artifacts.
The project pins Lean 4.35, and kernel checks used the installed 4.35 binaries via
a scoped `PATH`. The global `~/.local/bin/lake` remains at 4.34.1. No global
toolchain selection changed. At 353b215, hardened Lean, lean4lean (95,742
declarations), nanoda, con-leche, con-ron, and default kernel checks pass. The
final axiom set is `propext`, `Quot.sound`, and `Classical.choice`. Its scoped
sandbox shim creates a private `/run` for the existing `/run/user` mount on hosts
lacking that parent; it
does not weaken sandboxing or change host directories. The Rust nightly pin is
unchanged.

Resource measurements compare baseline `a3de4c4` with final source `088caa8`,
using the same corrected, trusted-denv fixture. Clean builds used fresh targets,
`KACHE_DISABLED`, and scoped GNU time. No cross-configuration comparison is used.

| Measurement | Baseline min / median / max | Final min / median / max |
| --- | --- | --- |
| Clean static CPU (s) | 49.71 / 52.30 / 53.94 | 52.53 / 53.06 / 55.63 |
| Clean static wall (s) | 35.99 / 37.64 / 38.56 | 38.34 / 38.75 / 40.75 |
| Clean static peak RSS (KiB) | 1,245,752 / 1,246,840 / 1,249,444 | 1,243,460 / 1,246,636 / 1,248,788 |
| Developer cold CPU (s) | 45.63 / 50.18 / 58.07 | 50.07 / 53.32 / 56.41 |
| Developer cold wall (s) | 9.19 / 10.00 / 10.82 | 9.88 / 9.90 / 10.48 |
| Developer cold peak RSS (KiB) | 401,696 / 402,400 / 404,584 | 394,140 / 397,380 / 397,744 |
| Developer warm CPU (s) | 0.03 / 0.03 / 0.04 | 0.03 / 0.03 / 0.04 |
| Developer warm wall (s) | 0.03 / 0.04 / 0.04 | 0.04 / 0.04 / 0.04 |
| Developer warm peak RSS (KiB) | 33,144 / 33,884 / 34,112 | 34,736 / 35,416 / 35,680 |

All five clean-static and developer-check pairs pass the 2x ceiling. CPU and wall
ranges overlap, while all peak RSS values remain within the ceiling. Static size
is 1,098,088 bytes at baseline and 1,170,120
bytes at final source, an increase of 72,032 bytes (6.56%). The dynamic artifact
is 1,111,296 bytes. Developer costs include the full all-target build, not an
isolated dependency cost.

All 14 runtime and allocation workloads pass five interleaved pairs of 31 samples
each, below 2x; median ratios range from 0.947 to 1.092. For denv, baseline median
latency is 93.125 µs (85.203–98.829) and candidate latency is 101.693 µs
(98.127–104.188). The ranges overlap, so these measurements establish no reliable
speedup. Allocation per timed operation changes from 714 allocations and 37,538
bytes to 711 allocations and 37,319 bytes. An initial single-transition sample
varied widely and does not establish stable latency. No optional optimization
gain is established.

The documented contracts now include the opt-in `command_policy` hook, wrapping
i64 arithmetic and explicit error boundaries, `Variables::bindings_bytes`,
`Shell::set_exit_status`, and strict `try_parse_word_parts` and
`try_parse_word_parts_bytes` helpers that preserve the legacy `Vec` helpers.
Command policy is opt-in; epsh remains unrestricted by default. The development
bridge enforces its byte-based JSON protocol, tagged outcomes, and explicit fuel
and unsupported-domain results. Its dev-only `serde_json` dependency validates
that protocol; version 1.0.154 is MIT or Apache-2.0 licensed and adds no production
dependency. `serde_core` is its only new compiled development transitive.
`regex-lite` reuses an existing rustybench transitive dependency for corpus
patterns. Process, descriptor, byte handling, and interactive behavior
are documented in the existing READMEs. The denv persistence change retains
atomic mode-0600 temporary writes, write-all, rename, and error rollback. It
changes no file format or crash-recovery contract.

The remaining hard stops are native macOS Apple silicon and Linux ARM64 hosts,
which are unavailable, and remote CI, which was not authorized. The campaign is
incomplete on those platforms.
