# Implement native `cp` and `mv` in ish

Status: ready for an execution request. This document does not start the campaign.

Deliver dependable native copy and move commands inside ish. Import useful code
and tests from the existing `../fs` checkout without changing that checkout or
keeping a build/runtime dependency on it. The detailed requirements under
[Reference](#reference) are normative acceptance requirements.

Deliver these capabilities:

1. A self-contained `crates/ish-fs` library with typed requests, anchored paths,
   exact destination mapping, collision policies, and complete-file publication.
2. Copy preservation and automatic native transfer strategies on Linux and macOS,
   with bounded traversal, descriptors, queues, buffers, and transfer records.
3. Immediate same-filesystem moves and durable cross-filesystem moves whose source
   retirement requires evidence of successful transfer, preservation, and durability.
4. Parent-process builtins supporting lists and redirections, with tested context
   refusal, foreground cancellation, and a controller shared with `trash`.
5. Behavior parity, fault and native platform tests, command-policy model updates,
   user/developer documentation, and reproducible performance evidence.

## Charter

Read `AGENTS.md`, the README, the verified code map below, and applicable local
instructions before editing. Follow the campaign mechanics and lane contract in
`/home/josh/usr/dotfiles/base/.agents/skills/orchestrate/references/campaigns.md`.
This document overrides commit-dependent mechanics with disjoint patch handoffs.

Decide technical details within the bounds below, append one short decision-log
entry for a consequential call, and continue. Start no optimization or polish
while a feature or required parity test is missing. Required clone/native paths,
bounded memory, and immediate rename are functional deliverables, not optional tuning.
Measure performance during parity, but apply final performance budgets only in P3.

Run until every deliverable passes or every remaining item is parked at a hard
stop. A hard stop parks that item, not independent work. Hard stops are missing
execution authority, unavailable required native host/filesystem, unsupported safe
OS primitive, or a loop that cannot check the affected behavior. Diagnose a failing
item with the root-cause method. After three refuted hypotheses, recheck premises
and bisect. After two such three-hypothesis rounds, give the integrator a minimal
repro and bounded redesign task. Park only if that task establishes a hard stop.
Never retry to green or loosen a requirement silently.

Fix an encountered correctness failure that blocks an item. Treat a focused warm
check over 30 seconds, a command over 120 seconds, or a reproducible regression over
3 times the baseline as a loop defect. Investigate in a dedicated slice before
relying on that command again. Cold compilation is recorded separately. Keep
checks under 10 seconds warm where possible. Kernel calls can be uninterruptible;
a 30-second watchdog reports the blocked operation rather than unsafely killing
threads or discarding data. Record unrelated defects and leave them outside scope.

Keep running one lane per disjoint ready ownership set, up to measured machine
capacity and the 32-child harness limit. Target at least four active lanes whenever
four independent slices are ready. The floor is the number of ready disjoint slices
when fewer exist. P0 permits three lanes (foundation, test inventory, docs/baseline
research), P1 permits six, P2 permits five, and P3 permits three. These are ownership
ceilings, not barriers. Start the next eligible slice as soon as its seam exists.
Replace or resume a stopped lane in the same turn. Overlap integration with the next
tranche and parallelize checks on disjoint immutable inputs. Do not run two Cargo
writers against the same target directory or change files underneath a running check.

Before each launch round run `PERF_GATE_LANE_GIB=4 perf-gate-ready -s`. Start with a
4 GiB per-lane reservation, then increase it to measured peak RSS rounded up to GiB
plus 1 GiB. Respect reported memory, swap, disk, CPU headroom, and available agent
slots. Exit 75 forbids adding width. Reserve at least 8 GiB disk per build lane.
The integrator resolves technical blockers and owns shared-file integration and
verification. Give every lane the largest coherent slice within its ownership.

All created worktrees and scratch live in `.work/wt/<lane>` and `.work/tmp`.
Confirm `.work/` is ignored through `.git/info/exclude` before writing there.
Set absolute `TMPDIR` under `.work/tmp`. Give each writer a disjoint worktree and
return reviewed patches without commits. HEAD alone is not the lane baseline:
it lacks previously integrated uncommitted seams. Seed each new lane with the
current integrated tracked and untracked snapshot, including this plan and removing
files already deleted in that snapshot. Pause integration while capturing the seed.
Copy only into the recorded task-owned worktree, excluding `.git`, `.work`, and
build artifacts. Never synchronize back from a lane with whole-tree deletion.

Keep a disposable alternate Git index under the integration repo's `.work/tmp`
for that seeded baseline. Populate it from HEAD, then stage the seeded snapshot
into that alternate index. Do not change the worktree's real index. At handoff,
copy the baseline index to another disposable index and stage only lane-owned paths
there, including new/deleted files. Use `git write-tree` on the baseline index and
`git diff --cached --binary <baseline-tree> -- <owned-paths>` with the handoff index
to produce only that lane's delta. These temporary indices/tree objects serve the
concrete no-commit handoff failure; they are not campaign provenance records.
Do not reuse a dirty baseline for a later assignment. Recreate the seed after
integration, or explicitly apply the prerequisite delta to the lane and refresh its
baseline before it resumes. Preserve and review any local lane delta first.

The integrator checks each patch with `git apply --check`, resolves overlapping
integration seams, applies accepted patches, checks the integrated state, and
follows orchestrate's worktree lifecycle. Preserve
uncommitted/untracked work and report its path. Without commit authorization, do
not claim a committed reboot-safe handoff or remove a worktree containing needed
work. If commits are later explicitly authorized, use Conventional Commits and put
item IDs in bodies. Never push.

## Decisions

### Owner-specified scope

The supplied draft specifies the exact-target dialect, `--into`, opt-in merging,
no overwrite by default, strict move preservation/durability, Linux and macOS,
and refusal of pipeline/substitution execution. Retain those choices. They are
requirements of the draft, not answers to new questions or authorization to run
filesystem operations on user data. No new owner product decision is pending.

### Prescribed decisions

- Use one `ish-fs` crate. Keep parsing/rendering in the ish adapter and platform
  primitives private. Pass shell cwd, raw arguments, environment-derived settings,
  cancellation, and handles explicitly. Existing epsh cwd is per Shell, not process cwd.
- Add an opt-in, owned byte-name placement table to epsh with a parent-process
  placement variant. Capture the evaluator owner PID at Shell construction and
  retain it across forks. Track logical substitution/pipeline context too, including
  substitution fast paths that avoid fork. Check resolved target placement before
  target redirections and before invoking captured policy/host callbacks. Resolve
  nested `command` and aliases through that same rule. Leave epsh's default policy
  disabled for existing embedders. This is an additive embedding API contract.
  Do not resolve the general callback-safety RFC or delete it as completed.
- Add `Exec::Foreground` to ish's registry for `cp` and `mv`. Lists/redirections
  use epsh, then the original-process handler. `trash` remains whole-line-only.
  Explicit executable paths keep access to external `cp` and `mv`.
- Accept redundant `cp -r` and `cp -R`; reject them for `mv`. Unknown options are
  status 2. Cancellation wins status selection (130); otherwise syntax/context is
  2, operational error is 1, and completed policy including skips/no-ops is 0.
- Without `--cross-file-systems`, encountering a nested mount is an operational
  error, not a successful empty subtree or implicit skip. Do not publish a new
  destination root from an incomplete traversal. An existing merge can retain
  completed leaves, and a move retains that operand's source. Explicitly traversed
  mountpoints that cannot be retired produce partial completion with retained names.
- If a hardlink group would require links across distinct destination filesystems,
  report an unrepresentable preservation requirement and retain affected sources.
  Do not silently substitute independent copies. Detect it before group publication
  where possible; earlier completed operands remain accurately reported partial work.
- Use one terminal/controller owner. During native operations service SIGINT and
  TERM/HUP through the self-pipe and use terminal input settings that deliver
  Ctrl+C. Keep SIGTSTP ignored; Ctrl+Z does not suspend native operations. Restore
  dispositions/settings on all exits. Keep external job control unchanged.
- Default transfers to `min(4, available_parallelism)` workers. Accept explicit
  jobs in 1..=32 only when descriptor and memory budgets permit; reject excess
  with status 2. Use one discovery producer, at most 256 queued work records,
  256 progress events, 256 KiB buffer per transfer, and 64 MiB combined in-memory
  traversal/hardlink/retirement record payload. Spill records before that limit.
  Bound descriptor use to `min(256, RLIMIT_NOFILE - 32)` and reduce scheduling
  concurrency, not safety, under that budget. If fewer than 16 descriptors remain,
  fail before mutations. Walk iteratively. Do not hold one descriptor per depth.
- Poll signals/events at most 50 ms apart while no kernel operation blocks. Start
  progress after 200 ms only on redirected stderr's actual terminal. Coalesce
  progress counters on queue saturation; never drop terminal outcomes. Continue
  draining while workers join. Broken output requests cancellation and yields an
  operational error unless user/SIGINT cancellation already determines status 130.
- Keep move completion as private validated state transitions. Only a durable
  transfer receipt can enter retirement; skips, same-entry no-ops, equality guesses,
  and errors cannot create that receipt. Capture/revalidate identities and version
  observations. Stable trees are the full-correctness baseline. Do not claim an
  atomic inode-and-version conditional rename or protection from arbitrary same-user
  writers. Detect observed changes, retain ambiguous objects, and report locations.
- Use a versioned, length-delimited raw-byte operation record only where interruption
  recovery requires it. Validate version, lengths, identities, and ownership; reject
  unknown/malformed records. Bound in-memory decoding. No general journal framework,
  persistent index, automatic sweep of old directories, or new recovery command.
- Preserve source uid/gid exactly for cross-filesystem move. Unsupported meaningful
  ACL/xattr/flags/ownership preservation fails and retains the source. Document
  timestamp rounding and nonsettable-field exclusions. Copy uses normal creation
  ownership and clears ownership-dependent privilege metadata as specified below.
- Use existing rustix/libc and scoped standard threads first. Do not import blake3,
  fs CLI, or sync/pruning machinery. A new dependency requires a demonstrated API
  gap, maintenance/license/transitive review, and P3 measurements within the gates
  below. The integrator alone owns manifests and lockfile.
- Keep the source's imported revision/local-change record in a single license/import
  note beside applicable MIT notices. This explicit import requirement justifies
  that record; create no checksum/provenance register or second campaign checklist.
- Native tests enter existing local test targets. CI workflow edits are excluded
  under the working contract. Existing macOS CI uses workspace tests; Linux CI
  uses `make test`. Adding the crate to default-members makes its native tests
  participate in both without workflow edits. This satisfies the draft's native
  CI coverage requirement through existing targets. A required unavailable native
  run stays blocked acceptance, never a passing hook.

Reverse a prescribed decision only when a focused experiment disproves its premise
and the replacement preserves every observable requirement. Log that evidence.
A change to product semantics, scope, preservation, or authority instead parks the item.

### Delegated decisions

The executor chooses internal names, sparse algorithms, metadata application order,
spool encoding details, and platform capability classification using local APIs and
real tests. Choose the smallest implementation meeting the stated guarantees.
Use Linux and macOS primitives independently behind one contract. If a guarantee
cannot be provided, fail explicitly and retain data. Do not broaden the product.

## Baseline and fast loop

Inspected on 2026-10-18: ish HEAD is `103c5938ff65d9d04fa9385d63c8f4e1236b5e05`.
Sibling fs HEAD is `abded69d49d7e3c449d1394fdf732c1c52bb230c`, clean.
The only initial ish change was this untracked document. Reinspect at execution;
these observations never authorize resetting to those revisions.

This shell's PATH omitted Rust tools. Prepend `/home/josh/.cargo/bin` here, or use
the installed Cargo directory on another host. Preserve `nightly-2026-09-15`.
The user Cargo configuration already selects `kache` and Linux's mold linker.
Use `PATH="/home/josh/.cargo/bin:$PATH" TMPDIR="$PWD/.work/tmp"` for the commands below.

| Existing check | Observed elapsed | Result |
| --- | --- | --- |
| `cargo test -p ish --lib dispatch::tests -- --nocapture` | 7.82 s cold, 0.05 s warm | 8 passed |
| `cargo test -p epsh --test embedding command_policy:: -- --nocapture` | 0.18 s | 6 passed |
| fs drift regression, isolated target under ish `.work/` | 7.35 s cold | 1 passed; ordinary copy retained wrong bytes, hash mode corrected them |

These are loop observations, not controlled performance claims. Tests completed in
under 0.03 seconds after compilation. The epsh manifest emits a pre-existing
`cargo::manual_readme` warning. No other baseline failure was observed in these
focused checks; the full suite was not run. The placement tests prove existing
policy ordering, not the new placement guard or safety of arbitrary child callbacks.
The real fs drift regression checks the import's critical semantic mismatch.
Reproduce it without writing the sibling checkout:

```sh
CARGO_TARGET_DIR="$PWD/.work/fs-target" cargo test --locked \
  --manifest-path ../fs/Cargo.toml --test semantics \
  hash_mode_detects_same_size_same_mtime_content_drift -- --exact --nocapture
```

Preflight host capacity was 32 CPUs, 54.9 GiB free memory and 635 GiB free disk.
Re-evaluate capacity at launch; these values are not reservations or timing evidence.
No existing end-to-end cp/mv performance baseline was measured. G2 prepares the
baseline fixtures and P3 captures production measurements before tuning.

At P0 time each selected focused check with `/usr/bin/time`, preserving existing
artifacts. Inventory actual test names before using filters. Tests that use a
filter must report a nonzero selected count. The following new targets are required
interfaces to create, not claims that they exist today. G0 creates their module
structure; their workstream owners implement real behavior and tests:

- `cargo test -p ish-fs --test policy <case> -- --exact`
- `cargo test -p ish-fs --test transfer <case> -- --exact`
- `cargo test -p ish-fs --test move_safety <case> -- --exact`
- `cargo test -p epsh --test foreground_placement <case> -- --exact`
- `cargo test -p ish --test pty native_fs_`
- `cargo test -p ish-fs --test native_linux <case> -- --exact` (Linux)
- `cargo test -p ish-fs --test native_macos <case> -- --exact` (macOS)

Use one named case per edit, then the touched target at integration. P1 ends with
all new targets and the existing dispatch, embedding, and trash regressions. P2
runs the workspace library/integration/PTY tests, lint, all 175 conformance cases,
and semantic verification once because shell dispatch and its models change.
P3 close reruns those gates only after substantive code changes since P2. Do not
run formatters or modify CI. Existing `make test` selects only default members;
add `ish-fs` to `default-members` and verify it is selected.

## Sequence

Each item owns its tests and related module docs unless shared files are listed.
S means about one focused session, M two, L several. Size never weakens acceptance.
The integrator alone edits root `Cargo.toml`, `Cargo.lock`, `Makefile`, `src/lib.rs`,
and the shared campaign/decision log. Other lanes propose those edits as patches.
Freeze the minimal requests, outcome types, platform signatures, and controller
messages in G0 before dependent implementation starts. G0 owns `platform/mod.rs`
and common metadata contracts; the integrator keeps ownership of those shared
seams after G0. E owns common engine metadata sequencing. L and A implement
platform metadata primitives in their respective platform modules. Keep all coupled changes
within their workstream. A lane can research tests/docs before its code seam exists.

| Phase/item | Size | Prerequisites | Exclusive ownership | Deliverable and narrow proof |
| --- | --- | --- | --- | --- |
| P0 G0 foundation | M | None | Integrator; `crates/ish-fs/src/lib.rs`, request/outcome contracts | Read current code, import selected reusable units/notices, define typed seams and crate selection; compiler plus real byte-path/no-replace regression, no stub success |
| P0 G1 parity inventory | S | None; no implementation until G0 | `crates/ish-fs/tests/`, fixture design | Inventory fs tests and every reference requirement; group cases by behavior, mark intentional differences and required native evidence |
| P0 G2 loop/baseline | S | None | Measurement/doc research only; shared edits to integrator | Time focused commands, note baseline failures/capabilities, identify exact native hosts and filesystem fixtures; no perf tuning |
| P1 P policy and roots | L | G0; G1 case inventory | `crates/ish-fs/src/policy.rs`, `roots.rs`, `tests/policy.rs` | All mapping/collision/options/ancestry/root/mount policies and independent logical-model comparisons |
| P1 L Linux primitives | L | G0 platform signatures | `crates/ish-fs/src/platform/linux.rs`, Linux native tests | No-replace, clone, partial native-copy fallback, sparse, metadata/sync primitives; actual filesystem tests plus faults |
| P1 A macOS primitives | L | G0 platform signatures | `crates/ish-fs/src/platform/macos.rs`, macOS native tests | FD clone, guarded 0444 cleanup, sparse data copy, ACL/xattr/flags and synchronization; native tests or explicit blocked native run |
| P1 E transfer and move | L | G0; integrate P/L/A seams as ready | `crates/ish-fs/src/engine/`, scheduler, records, transfer/move_safety tests | Complete staged copy, hardlink groups, rename-first/merge, durable transfer receipts, guarded retirement; every boundary fault case |
| P1 S shell placement | L | G0 outcome contract; engine stub only in tests | `src/builtin.rs`, `src/dispatch.rs`, new adapter; `crates/epsh/src/`, `crates/epsh/tests/foreground_placement.rs`, epsh API docs | Additive placement table, dynamic checks before callbacks/redirections, list/redirection semantics; existing embedding regressions and named placement tests |
| P1 C controller | L | G0 controller protocol; S wiring handoff | `src/foreground.rs`, `src/signal.rs`, `src/term.rs`, `src/trash.rs`; controller tests | Cancellation, event draining, terminal restoration, trash protections; deterministic lifecycle tests and trash regressions |
| P2 H hardening/integration | L | P,L,A,E,S,C feature code and parity cases integrated | Integrator owns `src/main.rs`, `src/job.rs`, `tests/pty.rs`, existing integration tests | Wire real engine/controller, complete all required fault/race/PTY/native evidence; no feature left as a mock |
| P2 V semantics | M | S placement behavior frozen; can start before H ends | `formal/`, `examples/semantic_bridge/`, verification docs | Model relevant policy changes, bridge positive/negative controls; focused policy family, then existing full verification |
| P2 D contract docs | M | Each landed feature, stagger with H/V | `README.md`, `docs/architecture.md`, `docs/ptytest.md`, `crates/epsh/docs/process-model.md`, new filesystem topic doc | All user/developer contract topics below, command examples/help/lookup/completion; documentation matches actual behavior |
| P3 B measurement | L | All features and parity tests complete, P2 gates pass | `benches/`, benchmark fixture/helper files | Engine and PTY workloads below, production comparisons, verify work/outputs; five interleaved runs per side where comparisons are claimed |
| P3 T tuning | M | B identifies a concrete bottleneck | Loan the affected workstream files exclusively; no concurrent writer | Optimize demonstrated limiter, preserve parity and resource bounds; rerun original workload and affected regressions |
| P4 X close | M | All prior items complete or explicitly parked | Integrator plus read-only review lanes | Prove dead paths unused, delete unused imported engines/dependencies, campaign-scoped ultrareview, fix P1/P2 findings, final checks/report |

P1 `native_linux` and `native_macos` test targets belong to L and A respectively; shared engine tests belong
to E. G1 hands its inventory to those owners and becomes available for independent
fault review. S hands the main-loop wiring patch to H. C hands the controller wiring
patch to H. D edits process-model documentation only after S releases its API docs.
No two lanes own the same file at once. Blocked native evidence does not prevent
reviewing or reporting completed code. X runs after independent work is exhausted,
even if earlier acceptance is parked. X cannot declare completion while acceptance
is blocked. P3 tuning still cannot start until feature/parity requirements pass;
a blocked native parity run permits benchmark harness preparation, not tuning.
The integrator supplies cross-stream changes
and resumes dependents immediately rather than waiting for a whole wave.

## Acceptance

Use the detailed reference matrices below as the parity checklist. Keep one compact
case table in the existing test fixtures, with behavior, case name, expected
source/destination/status, and deliberate difference from fs. Do not create a second
manifest or provenance log. No row may remain without a failing-if-wrong check.
Compare inherited behavior with fs fixtures; use this document's exact command
contract as the oracle for deliberate changes. Retain byte-preserving corpora.
Spot-check two plausible wrong implementations against real tests, then remove the
mutations. Do not normalize away meaningful metadata, bytes, or partial results.

| Deliverable | Required proof |
| --- | --- |
| 1, mapping and publication | `policy` target covers every mapping/collision matrix row below; native tests race a late target and a substituted temp name; replacement leaves other destination hardlinks unchanged |
| 2, data/preservation/resources | `transfer` and native targets exercise clone/native/buffered paths, sparse ends, restrictive access, ACL/xattr/ownership/privilege policy, invocation-wide hardlinks and constrained descriptors/memory; assert bytes, metadata, no leaked owned staging |
| 3, move | `move_safety` tests every numbered fault boundary below, explicit skips/new/changed source entries, self-induced metadata changes and all partial outcomes; genuine EXDEV plus synchronization trace; immediate directory rename proves zero descendant enumeration and zero transfer-worker initialization |
| 4, shell/lifecycle | `foreground_placement`, named `native_fs_` PTY cases, and trash regressions cover parent lists/redirections/argument substitutions, alias/expanded/nested-command child refusals before target effects, actual signals, saturated queues/broken stderr, joined workers and usable next prompt |
| 5, repository and docs | `cargo test --workspace --lib --tests`, `cargo clippy --workspace --all-targets -- -D warnings`, `make conformance`, `make verify`, and Linux `make verify-refinement`; no removed case IDs, axioms, weaker assertions, or invented durability proofs |

Report before expensive phase checks. Use the existing narrow commands for
iteration. Run native Linux and macOS tests on supported hosts before accepting
platform guarantees. Injected EXDEV/capabilities supplement real operations. Required
unavailable clone, mount, EXDEV, ACL or macOS evidence is a named blocked acceptance
item with a command and capability reason. Finish independent work. Do not claim
complete, native evidence, or release readiness while those requirements are blocked.
Do not provision remote hosts, privileged mounts, or crash the host without authority.
A disposable already-authorized VM may test storage crashes. SIGKILL proves process
interruption behavior only.

P3 measures every workload in the reference. Time gates require `perf-gate-ready`
exit 0 immediately before and `perf-gate-ready -1` immediately after each measurement.
Discard overlapping-load runs. After three busy readiness checks across useful work,
park measurements and report `perf gate deferred: host busy`. That is not a pass and
blocks declaring done. Use production release builds only here/final verification.
Compare five alternating runs per side, reporting median/range, CPU and elapsed
seconds, workload, cache state, methods and exact outputs. Use minimum CPU time for
compute gate decisions, and reject only regressions beyond observed spread.
No universal speed ratio is required. Native baselines must match mapping,
preservation and durability or be labeled as non-equivalent context.

Record baseline and candidate build CPU, binary size, startup/dispatch latency,
peak RSS, descriptor/queue/record maxima, and physical/logical copy accounting.
Final budgets: no new dependency increases representative warm build CPU or binary
size over 10% beyond spread without a measured necessary capability and a logged
justification; otherwise remove it. Native operations must stay within the stated
buffer/record/descriptor limits. Measure memory and binary size even when host load
blocks timing. Reject regressions over 3 times baseline outside spread. Smaller
regressions require a named preservation/durability cost or removal of the tuning.
Do not claim faster results without a named limiter and verified completed work.

## Execution boundary

The current request authorizes editing this document only. A later explicit request
to execute it authorizes repository implementation, local builds/tests, and bounded
disposable fixtures under `.work/`. It does not authorize commits, pushes, PRs,
publishing, installation, sibling edits, CI edits, user-data transfer/deletion,
privileged mounts, remote execution, or host crash tests. Use disjoint uncommitted
patch handoffs unless the owner separately grants local commits. Preserve unrelated
work. If authority is missing, park only that action and continue independent work.

Surface these planned contract changes in the final report: native `cp`/`mv` shadow
PATH commands, exact-target dialect and supported flags/statuses, metadata/mount/
partial/durability rules, `Exec::Foreground`, additive epsh placement API with default
behavior preserved, Ctrl+Z behavior, native-operation signal lifecycle, and the
operation record format if retained state is required. Update code, tests, help,
README, completion/lookup, architecture, process and verification docs together.
Document the dialect/options/examples, preservation and equivalence exclusions,
mount behavior, skips/statuses, partial outcomes, supported contexts, cancellation,
retained temporary state and manual inspection limits, and measured performance
limits. Do not leave docs claiming ordinary cp uses fs's metadata-skip policy.

## Non-goals

Keep ish interactive-only and execution/data structures flat. No async runtime,
daemon, surviving worker service, plugins/themes/prompt configuration, second parser,
ish-owned AST, scripting/POSIX mode, control flow, functions, background jobs,
`source`, `eval`, general config language, transaction framework, remote transfer,
Windows, undo/history feature, update/sync interface, callback broker, general
ISH-8 resolution, or rewrite of all fdu/epsh. Do not import fs CLI or sync/prune.
Do not create a benchmark-only command-string mode, compulsory source hashing,
precount pass, persistent acceleration cache, or global worker pool.

## Reference

Read the following current code and docs as implementation evidence, then use the
detailed requirements that follow as the behavioral contract.

| Area | Verified current locations and facts |
| --- | --- |
| ish registry/dispatch | `src/builtin.rs` has InShell, Hybrid, Native only; `src/dispatch.rs` rejects shell-only builtins inside lists; `src/main.rs` owns host dispatch |
| foreground | `src/trash.rs` scan/delete each create cancellation flags with no retained setter and block on recv; `src/signal.rs` ignores SIGINT and self-pipes WINCH/TERM/HUP; `src/term.rs`, `src/job.rs` own terminal/job behavior |
| epsh | `crates/epsh/src/eval.rs` checks expanded targets before redirections, but invokes captured callbacks in forked evaluators; `builtins.rs` handles nested command; `docs/rfc/ISH-8-callback-process-boundary.md` remains unresolved |
| destructive reuse | `crates/fdu-core`, `crates/fdu-scan`, `crates/fdu-delete/src/posix/quarantine.rs` provide descriptors, identity/mount observations, capture/revalidation and recovery; these do not establish transferred-version/durability evidence |
| fs extraction | `../fs/src/path.rs`, `engine/{copy,compare,traverse,sync,delete}.rs`, `platform/`, `metadata.rs`, `workers.rs`, `progress.rs` and `tests/` are current import references |
| fs mismatches | `../fs/tests/semantics.rs` drift case confirms metadata-only skipping; metadata docs exclude ownership/ACL/hardlink/sparse preservation; path resolution rejects intermediate symlinks; CLI uses -n for dry-run |
| fs publication | `../fs/src/engine/copy.rs` clone reopens RDWR before cleanup adoption; publication checks destination then ordinary rename; existing temp cleanup identity guard does not guard publication |
| native test selection | `.github/workflows/ci.yml` already runs macOS workspace tests and Linux `make test`; new default-member tests require no workflow changes |
| guidance | `README.md`, `docs/architecture.md`, `docs/ptytest.md`, `crates/epsh/{AGENTS.md,README.md,docs/architecture.md,docs/byte-preservation.md,docs/process-model.md,docs/testing.md}`, `formal/{README.md,PROTOCOL.md}`, `docs/verification/coverage.md`; sibling `AGENTS.md`, `README.md`, `docs/{architecture,safety,metadata,testing}.md` |

The fs metadata-skip behavior is deliberately superseded by unconditional transfer
for an allowed ordinary copy, without mandatory hashing. Do not carry that behavior
into move cleanup. Preserve applicable MIT notices and the one import revision note.
No final build/runtime dependency may reference the sibling, a submodule, an extra
fs executable, or a runtime download.

### Workspace and engine architecture

Start with one cohesive workspace library, for example `crates/ish-fs`. Naming can follow existing conventions. Separate four responsibilities:

1. **Ish adapter:** command arguments, shell integration, progress rendering, cancellation ownership, diagnostics, and exit status.
2. **Operation policy:** destination mapping, collision decisions, merge behavior, preservation requirements, and move completion rules.
3. **Engine:** descriptor-based traversal, scheduling, copying, metadata application, publication, and source retirement.
4. **Platform primitives:** cloning, native copying, descriptor operations, no-replace rename, ACL/xattr handling, and synchronization.

The library accepts typed requests and explicit root handles/context. It returns structured results and progress events/counters. It must not parse process argv, call `process::exit`, install process-global signal handlers, change cwd, mutate environment variables, or independently own terminal output.

Use owned/borrowed descriptor types and byte-preserving paths. Prefer rustix; isolate necessary libc/platform exceptions and explain unsafe invariants locally. Reuse buffers and existing dependencies where sensible. No global Rayon pool or other workers surviving a command.

Reuse fdu's useful enumeration, descriptor, identity, mount, batching, and destructive-operation patterns where they fit. A mandatory complete fdu index is not required for every copy. Do not force all operations through one universal traversal model. Same-filesystem rename needs no descendant scan; copy can stream; destructive cleanup needs records of what was actually transferred.

Keep shared extraction proportional to real reuse. Do not rewrite all fdu crates or epsh simply to make names uniform. Integrate the common foreground controller with `trash` without weakening its existing root protections, confirmation behavior, or recovery guarantees.

### Exact user-facing command contract

Implement these two operand forms:

```text
cp [OPTIONS] SOURCE DESTINATION
mv [OPTIONS] SOURCE DESTINATION
cp [OPTIONS] --into DIRECTORY SOURCE...
mv [OPTIONS] --into DIRECTORY SOURCE...
```

#### Destination mapping

- Two-path form always maps the source object to exactly `DESTINATION`. Never implicitly append the source basename because a destination directory happens to exist.
- Directories recurse automatically. Source or destination trailing slashes do not switch modes or cause a final symlink to be followed.
- `--into DIRECTORY` requires an existing directory and at least one source. Each source maps to `DIRECTORY/basename(SOURCE)`. Because this form explicitly selects a container, its directory operand may resolve through a final symlink to a directory; hold the resolved directory descriptor. This exception does not make a final symlink in the two-path destination form writable through its referent.
- Reject ambiguous basename spellings such as bare `/`, `.`, `..`, or a final `/.` or `/..` in `--into` mode. Explain how to use the exact-target form instead. Do not invent names through hidden canonicalization.
- Destination parent directories must exist. Creating an absent final directory is part of copying a directory; implicit `mkdir -p` is outside this command contract.
- Support `--` for leading-dash names. Preserve arbitrary Unix filename bytes, spaces, newlines, and control characters through parsing, expansion, execution, and records. Escape terminal diagnostics rather than emitting control characters literally.
- Preflight all top-level mappings before mutations. Reject duplicate output mappings, overlapping source operands that would interfere, and source/destination ancestry overlaps. Allow distinct source hardlink names when their output mappings are distinct.
- Do not lowercase or Unicode-normalize filenames to predict filesystem equality. Use actual filesystem behavior and exclusive publication to catch case-folding or normalization collisions.

`cp report.txt .` consequently reports a destination-type conflict; the insertion form is `cp --into . report.txt`. Document this deliberate difference prominently.

#### Collisions and merging

Use the following independent policies:

| Policy | Required behavior |
|---|---|
| Default | Existing target is an error. Do not prompt and do not overwrite. |
| `-n`, `--no-clobber` | Intentionally skip an existing target; a skipped move source remains. |
| `--replace` | Permit regular-file-to-regular-file or symlink-to-symlink replacement only. |
| `--merge` | Permit directory/directory recursion, preserving destination-only entries. Leaf collisions still use the selected collision policy. |

Reject `--replace` together with `--no-clobber`. Support `--merge --replace` and `--merge -n`.

Without `--merge`, an existing destination directory is a collision, including an empty directory. `--replace` never authorizes replacing a directory tree. File/directory and regular-file/symlink type conflicts fail; under `-n`, the existing target may be skipped without touching it. Destination symlinks are objects, never an implicit instruction to write through to their referents.

For merging, retain the explicitly configured ownership, mode, ACLs, and xattrs of preexisting destination directories. Ordinary child operations may naturally change their timestamps. Apply the source preservation policy to newly created directories after their child work completes. Do not surprise the user by chmod/chown operations on an existing merge container. This container policy is an explicit semantic exception to whole-object preservation.

The identical source/destination directory entry is a successful no-op. Distinguish that from two different hardlink names referencing one inode. For such distinct aliases, use the collision policy; reject `--replace` as a same-object conflict rather than claiming that a no-op rename moved anything. Never let a same-object/no-op result authorize source removal.

Known top-level conflicts selected for error by the collision policy prevent starting the invocation. Intentional `-n` skips are recorded and do not prevent processing other noncolliding mappings. Recursive conflicts discovered during an explicit merge can leave earlier completed changes; report them accurately. Do not claim a multi-source or merge transaction.

#### Supported options and status

Implement `--into`, `--merge`, `--replace`, `-n`/`--no-clobber`, `--dry-run`, `--durable`, `--cross-file-systems`, `-j N`/`--jobs N`, `-v`/`--verbose`, `--no-progress`, `-h`/`--help`, and `--`.

`--jobs` requires a positive, resource-bounded value. `--durable` strengthens copy publication; cross-filesystem moves already require that durability. Do not expose a flag that disables the move safety requirement. Accept `cp -r` and `cp -R` as documented redundant recursion flags. Reject them for `mv`. Do not emulate the entire GNU/BSD option surface. Unknown flags fail clearly and never silently delegate to an external command.

Use status 0 when the selected policy completed without errors, including explicit no-clobber skips and exact-entry no-ops; status 1 for operational failures/partial failures; status 2 for syntax or unsupported execution context; status 130 for cancellation. An intentional skip is a distinct outcome, never a transfer. If anything was skipped, provide a concise stderr summary even when ordinary successful operations would be quiet.

`--dry-run` resolves and reports source-to-destination mappings and intended actions without creating destination objects, temporary files, spools, or recovery records, or changing metadata. Report observations and predicted strategies, not guarantees that a later real run will see the same filesystem. Do not test clone support by creating a file during a dry-run. Do not promise that reads leave atimes or external filesystem bookkeeping unchanged.

Do not expose fs's metadata-convergence shortcut as the default or require hashing before an ordinary copy. A new update/sync interface is outside this campaign.

### Preservation policy

Define one documented logical preservation contract, applied consistently across clone, native-copy, and buffered paths.

#### Ordinary copy

- Preserve logical file contents and length, directory structure, symlink target bytes, timestamps representable by the destination, applicable source ACLs and xattrs/resource forks, and source hardlink relationships within the transferred source set.
- Newly created objects use normal creation ownership for the invoking user/destination directory. Do not require the user to chown a copy to the source owner merely to copy a readable file.
- Preserve ordinary source permission bits for final objects. Umask constrains initial creation; finalize ordinary rwx bits to the source policy, subject to the privilege normalization below. Apply representable source ACLs intentionally, replacing inherited entries when the platform permits, rather than silently combining them into broader access. If the required access semantics cannot be represented, diagnose that limitation. Clone and fallback must follow the same rules.
- Clear or otherwise safely normalize ownership-dependent privilege bits/capabilities when the copy's ownership differs. Do not blindly restore setuid/setgid bits after failed ownership handling. Security-relevant metadata that cannot be copied safely must produce a diagnostic.
- For an explicit file replacement, replace the selected directory entry atomically; do not truncate or modify the old destination inode. Other hardlinks to that old inode remain unchanged.

#### Cross-filesystem move

Preserve contents, symlink targets, source uid/gid under the documented move-equivalence policy, permissions, timestamps, ACLs, xattrs/resource forks, and hardlink relationships within the transferred set. If meaningful ownership cannot be reproduced, report the limitation and retain the source; permission failure is not authorization for a lossy move. Required preservation failure leaves the source intact. Do not silently downgrade because the target volume lacks a feature or a native API returns an error.

Distinguish absent metadata from metadata that exists but cannot be represented. Normal timestamp precision differences and platform-defined irrelevant fields are not failures to preserve identical raw bytes. Document the supported semantic equivalence. If meaningful source metadata cannot be represented or applied, fail explicitly; no implicit lossy move policy is required in this campaign.

Preserve captured atime and mtime to destination precision where supported, including supported symlink timestamps. Account for reads that update source atime; they are not evidence of external content mutation. Preserve creation time or platform flags where the supported API and policy allow it. Do not promise to reproduce ctime, inode numbers, identical physical allocation, or nonsettable/meaningless platform fields. Document any meaningful field excluded from the contract.

Hardlinks to names outside the transferred set cannot remain linked across filesystems; document that limitation. Coordinate hardlink groups across the full invocation, copy one representative, and create destination hardlinks for the others. A reflink is not a hardlink. Intentionally skipped names stay in the source and are excluded from successful retirement.

Preserve sparse regions where the destination supports them, including leading and trailing holes. Identical physical extent allocation is not required. A filesystem lacking sparse support may require dense storage; report that representation limitation and handle capacity failures without deleting source data.

Create staging files/directories with safe temporary access, perform child work, and finalize restrictive permissions, ACLs, timestamps, and flags in the correct order. Read-only source files and restrictive source directories must work when the user has sufficient read/search access. Never chmod a source or an existing destination merely to make a fast path succeed. Ensure clone-created metadata is normalized to the same policy as fallback-created metadata.

Copy supports regular files, directories, and symlink objects. Same-filesystem move may rename other filesystem objects without opening their contents. Unsupported cross-filesystem special-object transfer fails without opening FIFOs/devices as byte streams or deleting those sources.

### Paths, mounts, and publication

Resolve explicitly supplied parent paths, including ordinary intermediate symlinks, into held directory descriptors. After root establishment, operate relative to descriptors and single components. Preserve final source symlinks as objects and never follow symlinks discovered during recursion. Do not replace descriptor anchoring with canonicalized path strings.

Use no-follow opens, revalidate types and identities after opening, and avoid blocking on a FIFO substituted for a regular file. Bound symlink resolution. Reject copying a directory into itself or an ancestor/descendant alias, including through symlinked parents. For move, preserve protections for filesystem roots, home, the current directory, and its ancestors unless a supported operation can preserve shell invariants; V1 refuses these destructive source roots explicitly.

Mount restrictions concern traversal within a chosen source or destination tree. A source root and destination root on different filesystems must still support cross-filesystem copying and moving. Default recursive traversal stops at mount boundaries; do not mistake an empty-looking mountpoint for permission to replace or prune it. Use mount identities rather than only device numbers where the existing implementation supports them.

`--cross-file-systems` permits explicit traversal across nested mounts. It never authorizes unmounting or an unconditional deletion of mounted contents. Source retirement is still limited to transferred objects; unremovable mountpoint directories are retained and reported as partial completion. Add actual mount-boundary coverage where the environment permits it.

Use task-owned staging within the destination filesystem. For a new destination root, stage the complete file/tree and publish its root only after required work finishes. For a merge into an existing directory, publish completed leaves or absent subtrees independently; do not promise whole-merge atomicity.

For default/no-clobber publication, use kernel no-replace operations where available. An `exists()`/stat check followed by overwriting rename is unacceptable. If the filesystem cannot provide the required guarantee, fail safely or use a demonstrably safe primitive; do not silently weaken the policy.

For replacements, validate the expected kind and observed identity, and publish a completed new object. Protect staging names from substitution, verify the published object corresponds to the populated staging descriptor, and make cleanup conditional on ownership/identity. Never blindly unlink a name after an error. If a conflict prevents safe cleanup/restoration, preserve the ambiguous objects and identify their exact retained locations.

Write an honest concurrency contract. Descriptor anchoring, private staging, and revalidation do not provide an atomic conditional rename/unlink based on inode and mutation stamp on every filesystem. Stable source/destination trees are the full-correctness baseline; detect observable changes, refuse unsafe continuation, and document remaining races with arbitrary live writers. Do not claim security against an adversary with the same account's full authority.

### Fast transfer paths and bounded scheduling

#### macOS

Attempt FD-relative `fclonefileat` into a controlled staging name. If cloning is genuinely unsupported, use `fcopyfile` with appropriate data and sparse-copy behavior, followed by the common preservation policy. Keep a robust buffered fallback where necessary.

Do not pass `COPYFILE_CLONE` to `fcopyfile` and expect cloning; Apple documents that flag for path-based `copyfile`. Consult the [Apple copyfile contract](https://github.com/apple-oss-distributions/copyfile/blob/main/copyfile.3) for the distinction and `COPYFILE_DATA_SPARSE` behavior.

Fix clone handling for inherited `0444` modes and restrictive ACLs. Obtain only the descriptor access actually needed; if staging permissions must change, change only the unpublished object and restore intended metadata before publication. Install ownership/cleanup bookkeeping early enough to cover failures after successful clone creation. Consult [Apple's clonefile metadata rules](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2).

#### Linux

Attempt `FICLONE`, then appropriate `copy_file_range`/native transfer, then buffered copying. Use sparse extent information where available and a documented sparse-aware fallback. Establish the exact final logical length, including an entirely sparse file or a trailing hole.

Handle short reads/writes, short kernel copies, EINTR, zero-progress results, and unexpected EOF. A capability failure after partial copying may use fallback only with exactly correct source/destination offsets or a safe restart into the owned staging object. Never append a duplicate prefix, skip data, or report a short copy as complete.

Distinguish capability/compatibility errors from actual I/O, permission, quota, and ENOSPC failures. Do not bury real errors under endless retries or cache them as unsupported acceleration. Any capability cache is bounded, correctly scoped to the filesystem/object conditions, and discarded after the invocation. See the [Linux copy_file_range contract](https://man7.org/linux/man-pages/man2/copy_file_range.2.html).

#### Scheduling and accounting

Use separate bounds for directory discovery, active file transfers, descriptors, queued records, and worker buffers. Apply backpressure; wide or deep trees must not retain an unbounded list of paths or open directories. Preserve fs's useful bounded/spooled traversal principles, including its non-mutating dry-run path.

Tune conservatively. Do not copy the metadata scanner's large worker ceiling into data transfers. Do not hash all sources/destinations before trying clones. No compulsory preliminary size-count pass solely to draw a percentage bar. Progress may report discovered work until discovery completes.

Report logical bytes cloned, bytes streamed by the engine, files completed, and skipped/failed outcomes distinctly. Do not label logical reflink throughput as physical disk throughput. Native offload may not expose physical bytes; say so rather than guessing.

### Move execution and source retirement

Treat move as its own operation with explicit states. Never implement it as `copy(source, destination); remove_dir_all(source)` or as fs convergence followed by a fresh trash scan.

#### Rename first

Once top-level mapping, root protection, and collision policy allow it, attempt the appropriate whole-object rename immediately. A successful same-filesystem directory rename must not enumerate descendants, hash contents, or initialize a transfer pool solely for progress.

Only `EXDEV` triggers transfer fallback from an attempted rename. Permission errors, read-only errors, unsupported policy guarantees, collisions, and invalid ancestry remain their own errors. If implementing a no-replace rename through a compatibility wrapper, keep guarantee-support failures distinct from genuine cross-filesystem failure.

For explicit merge, rename entire nonconflicting subtrees when possible and descend only where directories overlap. Merging is an independently requested policy, not a fallback inferred from arbitrary rename failure. Same-filesystem renames preserve the moved object's metadata naturally; the existing destination-container merge policy still applies.

With `--durable`, synchronize the relevant source/destination parent namespaces after a rename. Do not walk and synchronize every descendant merely to perform a namespace move. A failure after rename must report that the rename occurred even if its requested synchronization failed.

#### Cross-filesystem transfer

Track, at minimum, source identity/stability observations, destination identity, copy method, preservation completion, publication completion, durability completion, skip/failure state, and source-retirement state. Make it impossible for an equality guess or generic `Ok(())` to authorize destructive cleanup.

For each source operand:

1. Establish anchored roots and record the objects actually selected for transfer.
2. Copy into destination-owned staging using the same logical policy as `cp`, with the move preservation requirements.
3. Complete required metadata and destination data/directory synchronization in a valid platform-specific order.
4. Publish complete destination entries or the completed new root, and synchronize the publication parent. Metadata/flags that can only be finalized after publication must finish before the durability barrier and before source retirement.
5. Complete copying and required durability for the transferable portion of this source operand before starting its destructive retirement. A real transfer/preservation/synchronization error prevents retirement for that operand. Intentional `-n` skips can coexist with retirement of successfully transferred entries.
6. Revalidate and retire only the recorded transferred source objects. Remove source directories only after their recorded children are handled and an empty-directory removal succeeds.
7. Synchronize affected source directory namespaces before claiming durable source removal.

Multiple operands can have partial completion. A merge can leave earlier published destinations if a later copy fails. Preserve those completed results and report their state; do not attempt a global rollback.

#### Guard source cleanup

Reuse/adapt the existing fdu capture-and-revalidate pattern where needed: claim a source entry under an operation-owned name with no-replace rename, verify that the captured object is the expected transferred object, then remove it. A mismatch requires safe restoration without overwriting another entry, or retention at a clearly reported location.

The copy version matters in addition to inode/type identity. Use appropriate source mutation observations before and after data/metadata work and before retirement. Account for expected metadata changes caused by this operation's own directory changes, hardlink retirement, and capture renames; do not misclassify every resulting ctime change as external modification. Do not explain away unrelated size/content/mtime changes as your own effects.

Newly created, skipped, failed, unsupported, changed, or untransferred entries remain. An unexpected new child must cause empty-directory removal to retain its parent. No fresh recursive source cleanup is allowed, even as an error-recovery shortcut.

Keep record storage bounded; use a scoped spool if the transferred-object record set is too large for memory. Any retained recovery record must contain enough identity/state to permit safe inspection or cleanup without blindly trusting a pathname. Do not create a background recovery service or automatically recurse through arbitrary old temporary-looking directories.

#### Failure and interruption states

Distinguish outcomes such as:

- Failed before publication; source retained.
- Destination published but durability failed; source retained.
- Destination durably completed; source changed and retained.
- Destination durably completed; source cleanup partially completed.
- Source removed; source-directory synchronization failed.
- Entry intentionally skipped; source retained.
- Cancelled, with explicit completed and retained counts/locations.

Once source retirement may have started, never clean up by deleting the only completed destination. Do not hide successful namespace mutations behind a generic failure message or claim they were rolled back.

The durability requirement is ordered synchronization of copied data, required metadata, and relevant directory entries. See the [fsync contract](https://man7.org/linux/man-pages/man2/fsync.2.html). Batch synchronization only where the dependency ordering remains valid. On macOS use the supported stronger regular-file synchronization mechanism appropriately; classify unsupported operations separately from real synchronization failures.

### Ish foreground execution and cancellation

Register each builtin once in the existing registry so completion, lookup, help, and execution agree. Preserve access to external tools through explicit executable paths. Do not silently choose a different implementation based on flags or whether a command is in a list.

The completed interface must support top-level commands, ordinary `&&`/`||`/`;` lists, and redirections through epsh's existing evaluation machinery. For example:

```sh
cp source destination && echo done
cp source destination || echo failed
cp first second; mv third fourth
cp source destination >output.log 2>errors.log
cp "$(printf '%s' source)" destination
```

A final implementation restricted to whole-line interception, like today's trash route, is insufficient.

Add a small explicit execution-placement contract, for example a parent-only foreground-operation class. Run its handlers only in the original shell/evaluator process. A process-context/owner-PID check belongs inside the evaluator's dispatch boundary, before target redirections and before invoking a captured host callback. Do not invoke an arbitrary host callback after fork merely to ask whether doing so is safe.

For this campaign, executing the new builtins as pipeline stages or inside command substitution is unsupported and must be refused with a clear diagnostic and status 2 before that target's effects. Cover static targets, aliases, expanded command names, and nested `command` dispatch. Parent-process expanded targets and `command cp` remain allowed. Argument substitutions are allowed because expansion finishes before transfer workers start.

Static policy checks may reject earlier; runtime checks must close dynamic bypasses. Preserve existing semantics for effects of earlier list commands and argument expansion. Do not require an enclosing `echo "$(...)"` to inherit the refused nested command's status if epsh normally does otherwise.

Scope this change to the needed placement contract and regressions. Do not build a callback broker, IPC service, second evaluator, or wholesale resolution of every ISH-8 concern. Update relevant process-model and verification documentation honestly.

#### Foreground controller

Use one controller for terminal state, progress rendering, cancellation, event draining, and worker completion. Reuse it for `trash` where appropriate while retaining trash's existing command contract.

- Workers receive explicit options, handles, and cancellation state; they never mutate shell state or cwd and do not read changing process environment.
- Worker lifetimes end before command return, the next list command, a subsequent fork, or shell environment/cwd mutation.
- Progress respects already-applied stderr redirection, appears only on an appropriate terminal after a short delay, and stays out of stdout.
- Normal quick success is quiet. Verbose mode reports completed mutations, while dry-run reports predictions. Summaries distinguish skips, errors, and retained sources.
- Broken stderr, a full bounded event queue, receiver shutdown, and worker failure must not deadlock shutdown. Continue draining events while joining workers.

#### Signals and terminal behavior

Connect Ctrl+C and SIGINT to cooperative operation cancellation. Use the existing self-pipe or another narrow async-signal-safe notification path. Signal handlers must not allocate, lock, print, modify terminal state, or invoke filesystem-engine logic.

The controller must continue servicing notifications during scans, copying, synchronization boundaries, and cleanup. Do not block indefinitely on `recv()` while the only cancellation logic sits outside that wait. A bounded timeout and self-pipe draining is acceptable; avoid a new event runtime.

On cancellation: stop scheduling new work, request active work to stop at safe boundaries, continue draining events, resolve owned temporary state, join every worker, restore terminal state, and return 130. Preserve sources whose corresponding destinations have not reached the required move state. Cancellation is cooperative; do not promise to instantly interrupt an uninterruptible kernel operation.

Integrate SIGTERM/SIGHUP with the shell's existing exit handling after requesting cancellation and cleanup. Do not make Ctrl+Z suspend only part of a threaded in-process operation or add a new background-job model. Document its supported behavior during native operations and preserve external-command job control.

Restore terminal settings, cursor/progress state, redirections, and prompt usability on every completion/error/interruption path. Existing trash confirmation, where retained, must also service cancellation without a permanently blocking input loop.

### Verification that exercises the actual guarantees

Carry over relevant fs tests and adapt their expected semantics. Use existing Rust test and PTY infrastructure. Add meaningful tests for data-loss boundaries, execution placement, and real platform operations; do not settle for tests that restate helper implementation details.

#### Policy and mapping matrix

Cover file, directory, symlink, dangling symlink, absent destination, and each type conflict across default, `-n`, `--replace`, and `--merge` combinations. Include:

- Exact-target behavior independent of destination existence and trailing slashes.
- `--into` mapping, multiple sources, duplicate outputs, ambiguous basenames, leading-dash names, and overlap rejection.
- Same directory entry versus distinct hardlink aliases.
- Raw/non-UTF-8 filenames and terminal-safe diagnostics.
- Same size and mtime with different bytes: `cp --replace` must establish source contents without a metadata-only skip; default copy reports the collision, and explicit `-n` may skip.
- Existing destination hardlinks: replacing one name leaves its other aliases unchanged.
- Existing merge-directory metadata retained; newly created directory metadata finalized after children.
- Non-mutating dry-run, including no temporary/spool creation.

Use a small independent logical model for the supported path/collision rules and compare representative generated cases against the real engine. Keep native syscall semantics tested on real filesystems as well.

#### Data paths and preservation

Exercise actual clone and fallback paths where supported, and deterministic test-only selection/fault hooks for paths the host cannot naturally select. Include:

- Empty and large files, partial reads/writes, zero progress, EINTR, ENOSPC, quota/permission failures, and metadata failures.
- Kernel-copy failure after partial progress, proving exact output bytes and length.
- Entirely sparse files, mixed extents, and trailing holes.
- Readable `0444` files, restrictive directories/ACLs, executable files, privilege-bit normalization, xattrs/resource forks, timestamps, and hardlink groups.
- An unsupported cross-filesystem special object, with no blocking open or source removal.
- Data and logical preservation equivalence through clone, native-copy, and buffered strategies.

Use genuine different-filesystem roots to test EXDEV on at least an available native environment. A hook that returns EXDEV supplements that evidence but does not replace it. Capability-dependent tests must report why they skipped. Add native macOS and Linux tests to the existing test targets. Run them locally on available native hosts. Do not edit CI workflows. Record unavailable native runs as blocked acceptance, with exact commands.

#### Move and publication boundaries

Use deterministic hooks/barriers, avoiding timing-only races, to test failure or cancellation:

1. Before copying and during a large buffered transfer.
2. After copy but before metadata completion.
3. During file/directory synchronization.
4. Before and after destination publication.
5. After publication but before parent synchronization.
6. After durable publication but before source retirement.
7. During source capture, removal, restoration, and source-parent synchronization.

Assert exact source/destination contents, names, metadata, temporary ownership, and structured outcome. Newly added source entries, replaced source leaves, changed contents, and skipped merge entries must not be deleted by cleanup. Test self-induced directory/hardlink metadata changes so legitimate retirement does not falsely fail.

Test atomic no-replace against a destination created after preflight, destination symlink substitution, temporary-name substitution, and safe cleanup when an expected name has changed. Preserve ambiguous data rather than deleting it to make a test clean up successfully.

Use process-kill tests for process-interruption behavior. They do not prove power-loss durability. Verify synchronization ordering with an operation trace/fault injection. Actual storage crash guarantees need a suitable disposable VM/filesystem harness; do not perform destructive host crash tests or mislabel SIGKILL evidence.

#### Shell integration and lifecycle

PTY/integration tests must cover successful/failing list continuations, redirections, a failed redirection preventing transfer, parent-process `command cp`, expanded command names, argument substitutions, and static/dynamic refusals in forked contexts before target effects.

Cancel during scanning, copying, and move completion. Verify return status, a usable next prompt, restoration of terminal state, and no workers surviving into a subsequent external command, pipeline, substitution, or environment mutation. Include event-queue saturation and output failure during shutdown. Preserve existing external signal/job-control behavior.

#### Existing formal and repository gates

Run applicable workspace, integration, PTY, lint, conformance, and semantic verification gates specified by current repository instructions. Include the new library in the normal build/test workflow; verify that workspace/default-member choices do not accidentally omit its tests.

Update affected command-policy models and bridge cases. Do not weaken the existing conformance corpus, mutation tests, theorem checks, or assertions to get green results. Do not add axioms, admitted theorems, or claims that Lean proves operating-system durability. The formal boundary remains what the model actually covers.

### Performance evidence and tuning

Add a reproducible benchmark entry point integrated with existing conventions. Use disposable fixtures under an explicitly designated temporary root and verify teardown ownership. A POSIX shell orchestrator with Rust helpers or the existing benchmark framework is sufficient; no new Python/Perl dependency is needed.

Measure both engine work and representative actual builtin execution. Do not add an ish command-string/script mode just to benchmark it; use the existing PTY harness for the interactive path.

Cover:

- Repeated tiny copies and moves for startup/dispatch overhead.
- Flat directories of many small files and one large nested subtree.
- Deep/wide trees under constrained descriptor and memory budgets.
- Large real byte copies, clone-capable copies, and sparse files.
- Same-filesystem directory moves, proving no descendant walk on the simple rename path.
- Cross-filesystem moves, separating transfer, destination synchronization, and source-retirement time.
- Explicit merges with large nonconflicting subtrees.
- Progress enabled/disabled on a terminal.

Compare equivalent destination mapping, preservation, and durability policies. Include Apple `cp` and `cp -c`, GNU clone-aware `cp`, and relevant native `mv` baselines. Current GNU cp already uses reflink automatically; see its [manual](https://www.gnu.org/s/coreutils/manual/html_node/cp-invocation.html). Native utilities may offer weaker durability, so label that difference rather than crediting it as equivalent work.

Record platform, filesystem, fixture shape, input sizes, cache conditions, worker settings, methods actually selected, and variation across repeated measurements. Separate logical cloned size from streamed data and unknown physical I/O. Do not require privileged cache dropping or invent cold-cache results.

Profile concrete regressions and tune demonstrated bottlenecks. Performance matters, but universal speedup or a fixed ratio on a noisy host is not an acceptance gate. Structural regressions such as prewalking a renamed tree, unbounded queues, duplicate copying, or hashing every ordinary clone candidate must be fixed.


## Decision log

Append one dated line per consequential executor judgment with the call and reason.
No execution decisions have been recorded.

## Campaign log

Append one line per phase end with passed or blocked acceptance and the next action.
Use this section as the campaign checklist. No campaign execution has started.

## For the owner afterwards

- Read the README filesystem contract, the additive epsh API/process documentation,
  the preservation and retained-data limits, and the final acceptance report.
- Review the working diff and any preserved worktree paths. No cherry-pick is needed
  unless local commits were separately authorized. Then report the exact branches.
- Read representative measurements and every blocked native/performance gate before
  deciding whether to release. Owner review is not an implementation acceptance gate.
