Prefix: ISH
Next: 23

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

### ISH-22 · P2 · History tail recovery leaves concurrent readers on stale occurrence IDs

- **Area:** Data
- **Where:** `src/history/store.rs:294`, `src/history/store.rs:414`, `src/history.rs:128`
- **Problem:** Final-frame recovery truncates without changing the store generation. A focused regression loaded two entries into an existing reader, corrupted the final frame, and opened a fresh reader that retained one entry. After recovery and an external append reusing occurrence ID 2, the existing reader still contained the discarded second command and could not see the new external command.
- **Recommendation:** Publish recovery that removes previously observable occurrences as a new generation, atomically replacing the retained prefix so readers rebuild their snapshots and reset occurrence cutoffs.
- **Fix with:** durable-state
- **Effort:** M (days)

### ISH-23 · P3 · Add a builtin to copy the last command's output

- **Area:** Architecture
- **Where:** `src/main.rs:256`, `src/main.rs:302`, `src/main.rs:2664`, `src/builtin.rs:26`, `README.md:128`
- **Problem:** `copy-scrollback` copies terminal history, but ish cannot copy only the previous command's output: foreground children inherit the terminal and write around ish, while `TermWriter` contains only ish's UI bytes. Capturing output therefore needs an execution-path change, not just another builtin.
- **Recommendation:** Add the standalone `?` builtin and retain the most recent interactive command's output in a reusable, bounded in-memory byte buffer while relaying it live to the terminal. Do not use temporary files or helper subprocesses. Preserve interactive child behavior while choosing the relay mechanism; a pipe can change `isatty` and child buffering, so evaluate a PTY relay if that matters. Reuse the existing OSC 52 clipboard path. Specify whether capture includes stderr and how pipelines and explicit redirections affect the captured bytes; bound retained memory for large or unbounded output.
- **Fix with:** lifecycle-concurrency, os-boundaries
- **Effort:** M (days)
