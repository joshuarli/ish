# ISH-5: Foreground job outcomes

Status: accepted; implemented. Linux and native macOS PTY and process tests
passed.

## Decision

Add a structured program-execution API that returns either completion or a
stopped foreground job. Keep `run_program` and `run_script` as numeric
compatibility adapters. epsh owns child tracking, waits and cleanup; ish owns
the prompt, saved terminal modes, output capture and one suspended-job slot.
Resuming a job never resumes the remainder of its original command list.

This remains the implementation contract for ISH-5 in [TODO](../../TODO.md).
The shipped API and process ownership are described in epsh's process model.

## Why

`eval_pipeline` currently keeps member PIDs internally, but `run_program`
reduces `ShellError::Stopped` to 148. ish can recover only stops reported by
its separate external-handler thread-local slot. `job::resume_job` waits for
one group member, losing pipeline completion semantics. It already retains
the job on most resume/wait errors; that behavior must extend to the whole
tracked group, not be replaced with unconditional slot removal.

A second wait implementation in ish would keep these paths divergent. A
numeric status cannot establish child ownership: a command legitimately
exiting 148 must never adopt an earlier stopped job.

## Ownership and outcomes

- The stopped outcome carries an opaque, non-cloneable epsh job handle. Its
  state records the owning process, PGID, every direct child still owned,
  completed member statuses, final-stage identity, applicable status rules
  and any epsh output relays. Raw PID/PGID values are diagnostic data, not a
  second independently waitable handle. Reject use from a different Shell or
  process; a copied child-side destructor must not signal the parent's job.
- Use the same tracker for a standalone external command and a pipeline.
  Add an additive external-handler result/path for "not handled", and let
  ish's handler decline ordinary external programs to epsh's default executor.
  ish retains its in-process command handlers but removes its duplicate
  external `spawn`/`waitpid` path and stop thread-locals. Existing status-only
  handlers remain compatible and own any processes they create; they cannot
  manufacture an epsh-owned resumable job from raw integers.
- On a stop, bring the dedicated foreground group to a stable state: stop
  remaining live members and observe each tracked direct child as stopped or
  exited before returning the prompt. Preserve statuses already collected.
  Completion requires reaping every owned direct child, not the first exit.
  Default pipeline status remains the final stage's; epsh's existing
  `pipefail` and negation semantics must also survive a stop/resume cycle.
- epsh's interactive lifecycle path performs terminal PGID handoff and
  reclamation. Before displaying the prompt, ish saves the stopped job's
  termios and restores shell modes. `fg` restores the job's modes before
  continuing it. Reclaim the terminal on completion, stop, cancellation and
  errors. Keep output relays/capture alive while stopped; finish them only
  after completion or explicit termination.
- Provide resume and terminate/reap operations through epsh. Retry EINTR;
  preserve the handle and unconsumed statuses on recoverable wait/handoff
  errors. Treat ECHILD per member: report lost ownership, remove only that
  member, and finish accounting for the rest. Never signal a PGID after its
  owned members have all been relinquished or reaped.
- Cancellation, timeout and setup-failure cleanup target only the current
  operation's owned children. They must not terminate a previously suspended
  job or reap unrelated embedder children. Explicit abandonment and dropping
  an outstanding handle terminate its group and reap owned direct children;
  cleanup cannot leave a stopped group waiting for SIGCONT indefinitely.

## User-visible and compatibility choices

1. Stopping a command discards the remaining `&&`, `||` and `;` list, as the
   README already specifies. `fg` resumes the process group only; no AST
   continuation, delayed list evaluation or additional job table is added.
2. Keep the existing one-job policy: if the slot is occupied, report that fact
   and continue the new foreground group instead of replacing the saved job.
   If continuing it fails, terminate/reap the new group and preserve the old
   slot. Apply the policy equally to standalone commands and pipelines.
3. Numeric adapters cannot transfer job ownership. If their structured result
   is stopped, terminate/reap that foreground job, emit an explanatory
   diagnostic and return the existing documented status 148. Interactive
   embedders that need suspension must use the structured API. Completed
   programs, explicit exit, runtime errors and trap behavior retain their
   existing contracts; a stop does not run EXIT traps.

## Implementation and acceptance

Implement the epsh group tracker and structured API first, migrate ish's
external dispatch and `fg`, then remove the numeric/TLS stop transport. This
does not depend on moving ish's `Shell`; ISH-48 preserves the same ownership.
Callback changes must obey [ISH-8](ISH-8-callback-process-boundary.md).

Required Linux and macOS coverage:

- A multi-stage pipeline stops, appears in the single slot, resumes, stops
  again and completes with the final stage's actual status. Include an early
  exit and a final stage that exits before another member; reap every member.
- A real exit 148 cannot create a suspended job. Resuming a stopped list never
  runs its discarded remainder. Check epsh `pipefail` and negation separately.
- An occupied slot survives a second stop, a failed continue, cancellation of
  a different command and injected EINTR/non-ECHILD wait failures.
- Terminal ownership/modes and captured output survive repeated stop/resume;
  no output-relay join waits for EOF while the job remains stopped.
- Numeric adapters, explicit termination, shell exit and dropped handles leave
  no owned stopped children or zombies. An unrelated child remains waitable
  by its original owner. Use process/PTY evidence, not sleeps alone.
