Prefix: ISH
Next: 4

### ISH-1 · P3 · Preserve buffered input between prompts

- **Area:** Data
- **Where:** `src/main.rs:698`, `src/main.rs:708`
- **Problem:** `read_line` creates a new InputReader for every prompt, discarding bytes buffered after Enter. A PTY write containing two complete commands executes only the first; the review reproduced this with two file-writing commands.
- **Recommendation:** Retain the input reader across prompts and test two commands delivered in one write.
- **Fix with:** root-cause
- **Effort:** S (hours)

### ISH-2 · P3 · Reject malformed detached HEAD text without panicking

- **Area:** Errors
- **Where:** `src/prompt.rs:294`
- **Problem:** The detached HEAD display truncates UTF-8 at byte eight. A checkout whose HEAD contains `ééé€x` makes shell startup panic because that boundary splits a character; reproduced against the running shell.
- **Recommendation:** Validate detached object IDs before abbreviating them, treating malformed metadata as unavailable Git status.
- **Fix with:** os-boundaries
- **Effort:** S (hours)

### ISH-3 · P3 · Allow signals to interrupt discarded oversized pastes

- **Area:** Concurrency
- **Where:** `src/input.rs:337`
- **Problem:** Source review found that `skip_paste_remainder` polls stdin indefinitely while waiting for the paste terminator. It does not poll the signal descriptor, so an unfinished oversized bracketed paste can prevent pending termination signals from reaching the main loop. Runtime impact remains unproven.
- **Recommendation:** Use the same signal-aware input wait as ordinary input and propagate interruption while discarding the paste.
- **Fix with:** lifecycle-concurrency
- **Effort:** S (hours)
