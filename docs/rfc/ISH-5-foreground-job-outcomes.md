# ISH-5: Foreground job outcomes

Status: draft

## Summary

Preserve stopped pipeline identity across epsh evaluation and ish job control.
Define who waits for every member before adding another job-control path.

## Motivation

`Shell::eval_command` can return a stopped PID and PGID, but `run_program`
converts that result to status 148. ish uses `run_program` and records stops
through a separate parent-process slot populated by its external handler.
Pipeline stops do not populate that slot. The prompt returns with stopped
children that `fg` cannot find. Waiting for just one member during resume would
also leave the lifetime of the remaining pipeline undefined.

## Design

Introduce a structured program outcome that distinguishes completion from a
stopped foreground group. A stopped outcome must preserve enough information to
resume and reap every member, restore terminal state, and compute the last
stage's eventual status. A numeric status alone cannot carry that ownership.

Keep child tracking and wait bookkeeping in epsh. Let ish own terminal UI and
its single suspended-job slot, using an explicit epsh resume/termination
operation for tracked members. Define when terminal ownership transfers back to
ish and when a stopped outcome transfers responsibility for later cleanup.
Neither layer may independently reap the same member.

The outcome must specify what happens to the remainder of an AND-OR or
sequential list when execution stops. Preserve the existing one-suspended-job
limit, including cleanup when replacing a suspended job. Store terminal modes
with the suspended foreground job where needed by the existing terminal path.

## Alternatives considered

- Add a structured entry point and retain the numeric API as an adapter. This
  limits public API disruption, but the adapter must define stopped-job cleanup.
- Change `run_program` to return the structured outcome. This makes ownership
  visible to every caller and requires a public API migration.
- Loop over `eval_command` in ish. This duplicates program-level status, trap,
  and error handling and leaves pipeline-member ownership unresolved.

## Open questions

1. Should the existing numeric API remain? Recommend a structured entry point
   with a numeric adapter that has explicit cleanup semantics; alternatively,
   accept a breaking return-type change.
2. Should a resumed group continue its interrupted list? Recommend preserving
   current shell behavior initially and documenting the continuation boundary;
   implementing resumable list evaluation requires a separate explicit contract.

## Decision

Pending. Implementation must cover a stopped multi-stage pipeline, repeated
stop/resume, early member exit, termination, and reaping all members without
losing the final stage's status. Move the resulting contract into the process
model and delete this RFC when implemented.
