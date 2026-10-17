Prefix: ISH
Next: 22

### ISH-5 · P2 · Stopped pipelines disappear from foreground job control

- **Area:** Concurrency
- **Where:** `src/main.rs:308`, `src/main.rs:319`, `crates/epsh/src/eval.rs:1161`, `crates/epsh/src/eval.rs:1936`, `src/job.rs:41`
- **Problem:** epsh reports a stopped pipeline with PID and PGID, then run_program discards those IDs and returns 148. ish records a suspended job only from its parent-process STOPPED_JOB slot. In a fresh-build PTY, stopping a pipeline returned the prompt, but `fg` reported no suspended job while the children remained stopped. The current resume path also waits for only one group member.
- **Recommendation:** Preserve the structured stop outcome through the embedding boundary and define ownership of every pipeline member during stop, resume, cancellation, and reaping.
- **Fix with:** lifecycle-concurrency, error-model
- **Effort:** M (days)
- **Design:** [Foreground job outcomes](docs/rfc/ISH-5-foreground-job-outcomes.md)

### ISH-8 · P2 · Forked external callbacks can deadlock on captured host locks

- **Area:** Concurrency
- **Where:** `crates/epsh/src/eval.rs:713`, `crates/epsh/src/eval.rs:1622`, `crates/epsh/src/eval.rs:2063`
- **Problem:** ExternalHandler is Send but can capture a mutex held by another host thread when command substitution forks. The child invokes the copied callback with a permanently locked copy. A fresh-source library probe completed with status 0 when unlocked; when held across fork, the parent thread released at 99 ms but the child stayed blocked until the one-second shell timeout returned 130.
- **Recommendation:** Define and enforce the callback execution boundary for forked evaluators. Audit command-policy and redirect-open callbacks under the same boundary rather than treating Send as a fork-safety guarantee.
- **Fix with:** lifecycle-concurrency, api-surface-and-module-boundaries
- **Effort:** L (weeks)
- **Design:** [Callback process boundary](docs/rfc/ISH-8-callback-process-boundary.md)

### ISH-15 · P2 · Safe library APIs conceal environment mutation preconditions

- **Area:** Security
- **Where:** `src/lib.rs:3`, `src/lib.rs:7`, `src/lib.rs:11`, `src/denv.rs:96`, `src/denv.rs:101`, `src/denv.rs:106`, `src/denv.rs:841`
- **Problem:** Public safe wrappers call unsafe std environment mutation without enforcing its Unix process-wide concurrency preconditions. Public denv operations also reach those mutations. Safe library callers can invoke them while another thread uses libc environment readers. The pinned standard-library safety contract explicitly names such readers, including DNS resolution. This is a source-proven missing safety boundary; a concrete undefined-behavior manifestation is unproven and was not attempted. Normal ish foreground execution and joined relay threads do not establish an observed incident.
- **Recommendation:** Expose the required safety contract through every public mutation entry point, or remove process-environment mutation from the library interface in favor of explicit environments. Changing only the three wrappers leaves the denv boundary unresolved.
- **Fix with:** os-boundaries, api-surface-and-module-boundaries
- **Effort:** M (days)
