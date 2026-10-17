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






### ISH-14 · P2 · Denv trust filenames reject ordinary long project paths

- **Area:** Data
- **Where:** `src/denv.rs:400`, `src/denv.rs:468`
- **Problem:** The trust key hex-encodes the entire canonical path as one filename, doubling its byte length. On a filesystem with a 255-byte component limit, a public-API probe using a 134-byte .env path made `denv allow` fail with Filename too long and status 1. Such paths need no unusually long individual directory name.
- **Recommendation:** Use a bounded digest of the canonical path for the trust filename, retaining the existing content validation and checking existing trust-format migration requirements.
- **Fix with:** os-boundaries, durable-state
- **Effort:** S (hours)

### ISH-15 · P2 · Safe library APIs conceal environment mutation preconditions

- **Area:** Security
- **Where:** `src/lib.rs:3`, `src/lib.rs:7`, `src/lib.rs:11`, `src/denv.rs:96`, `src/denv.rs:101`, `src/denv.rs:106`, `src/denv.rs:841`
- **Problem:** Public safe wrappers call unsafe std environment mutation without enforcing its Unix process-wide concurrency preconditions. Public denv operations also reach those mutations. Safe library callers can invoke them while another thread uses libc environment readers. The pinned standard-library safety contract explicitly names such readers, including DNS resolution. This is a source-proven missing safety boundary; a concrete undefined-behavior manifestation is unproven and was not attempted. Normal ish foreground execution and joined relay threads do not establish an observed incident.
- **Recommendation:** Expose the required safety contract through every public mutation entry point, or remove process-environment mutation from the library interface in favor of explicit environments. Changing only the three wrappers leaves the denv boundary unresolved.
- **Fix with:** os-boundaries, api-surface-and-module-boundaries
- **Effort:** M (days)

### ISH-16 · P3 · Newlines in project paths break denv restoration records

- **Area:** Data
- **Where:** `src/denv.rs:536`, `src/denv.rs:563`
- **Problem:** Active state stores the project path as an unescaped first line. A newline in a valid Unix directory name shifts the remaining fields and load_active silently treats the record as absent. A public-API probe entered and left a trusted project whose name contained a newline; its scoped environment marker remained set.
- **Recommendation:** Encode paths losslessly in a versioned active-state format and distinguish malformed state from absent state before applying or restoring project values.
- **Fix with:** durable-state, os-boundaries, error-model
- **Effort:** S (hours)

### ISH-17 · P3 · Oversized terminal parameters overflow the input decoder

- **Area:** Errors
- **Where:** `src/input.rs:453`, `src/input.rs:462`
- **Problem:** The CSI decoder accumulates decimal parameters with unchecked u32 multiplication and addition. A fresh debug-build PTY fed an oversized decimal CSI parameter panicked. Release arithmetic can wrap instead; that release outcome is inferred from the arithmetic and build profile, not separately reproduced.
- **Recommendation:** Use checked accumulation and consume or reject the complete malformed sequence without panicking or interpreting a wrapped parameter.
- **Fix with:** error-model
- **Effort:** S (hours)

### ISH-18 · P3 · Public empty pipeline AST panics during evaluation

- **Area:** Types
- **Where:** `crates/epsh/src/ast.rs:24`, `crates/epsh/src/eval.rs:1849`, `crates/epsh/src/eval.rs:1855`
- **Problem:** The public Pipeline variant accepts an empty Vec, but evaluation subtracts one from its length. A library probe constructing this public AST caught a debug panic. The parser does not create empty pipelines, so this affects embedders constructing ASTs rather than normal parsed ish input.
- **Recommendation:** Reject empty pipelines at the public evaluation boundary or represent pipeline stages with a nonempty type, with an explicit public API decision if the AST changes.
- **Fix with:** make-invalid-states-unrepresentable, api-surface-and-module-boundaries
- **Effort:** S (hours)

### ISH-19 · P3 · Tab-separated cd history loses its original directory target

- **Area:** Data
- **Where:** `src/main.rs:1335`, `src/frecency.rs:48`
- **Problem:** History target resolution recognizes only literal `cd ` and `z ` prefixes, although parsing accepts tabs. A fresh-build PTY successfully ran a pasted `cd` followed by a tab and `child` from directory A. After moving to B, `z child` failed because history retained the relative target and resolved it under B instead of the existing A/child.
- **Recommendation:** Resolve directory-change history using the accepted lexical command boundary and arguments, including tabs, before recording the absolute target.
- **Fix with:** none
- **Effort:** S (hours)

### ISH-20 · P3 · Prompt retains a no-repository result after repository creation

- **Area:** Data
- **Where:** `src/prompt.rs:150`, `src/main.rs:308`, `src/main.rs:2272`
- **Problem:** The prompt caches a negative Git lookup until directory invalidation. A Prompt API probe rendered outside a repository, created .git/HEAD in that same directory, and rendered again: the branch remained absent until explicit invalidation. Running git init without changing directory follows the same cache lifecycle.
- **Recommendation:** Revalidate a cached negative lookup after commands that may change repository state, or use bounded filesystem freshness checks without spawning Git.
- **Fix with:** isolate-effects
- **Effort:** S (hours)

### ISH-21 · P3 · Default epsh installation exposes conformance fixture binaries

- **Area:** Build
- **Where:** `crates/epsh/Cargo.toml:17`, `crates/epsh/Cargo.toml:35`
- **Problem:** The package declares emit_hex, show_argv, show_env, and stat_path as normal binaries. An offline debug `cargo install --path crates/epsh` into an isolated root installed all four alongside epsh. A default user installation therefore adds test utilities to the executable namespace.
- **Recommendation:** Keep fixture executables available to integration tests without including them in the default published installation surface. The README now selects `--bin epsh` as an immediate user-facing mitigation.
- **Fix with:** api-surface-and-module-boundaries
- **Effort:** M (days)
