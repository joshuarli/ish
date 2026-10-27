# ISH-62: Capture input tail

Status: draft

## Summary

The converged-capture relay must not discard terminal input it has already read. Hold the unwritten tail until the capture master accepts it, or return it to the editor when the command is gone.

## Motivation

`Pump::forward` reads up to 16 KiB from the real terminal only after the master looks writable. `write_master` can still accept only a prefix. It then gives up when the master is still unwritable after 200 ms, when the stop pipe is readable, when forwarding is paused, or when the write returns any other error. `forward_chunk` drops the rest of the chunk.

For a converged command the real terminal is raw, and the relay is what turns `VINTR` and `VQUIT` into signals or forwards them to the slave. A dropped byte is not delivered. A paste that fills the slave input queue can swallow a following Ctrl+C. Bytes read as the command exits never reappear at the next prompt.

The kernel cannot put those bytes back. The relay has to remember them.

## Design

Keep one unwritten input buffer on the relay, bounded by the existing read chunk.

- While the session is live and not paused, retry that buffer before reading more from the real terminal. A full slave queue delays the next input read. It does not discard bytes, and it does not by itself stop output already buffered on the master.
- On pause for a stopped job, keep the tail with that session and write it after `fg` resumes forwarding.
- On session end, if the child has exited, pass the tail to the editor as pending input for the next prompt. Do not write it back with `TIOCSTI`.
- Apply suspend, interrupt, and quit handling to the tail before any new read, using the same rules as `forward_chunk`.

Output relay failure stays a reported relay error. It is not a reason to drop input that was already taken from the user.

## Alternatives considered

- Block the whole relay, including output, until the slave queue drains. That preserves input but stalls the screen for a program that is not reading.
- Leave the current drop. The user-facing failure is a lost interrupt and lost type-ahead, which the previous comment denied.
- Push the tail back onto the real terminal with `TIOCSTI`. That ioctl is restricted and is not a portable contract.

## Open questions

- Should a full slave queue delay only further input reads, as recommended, or also pause output so a slow reader cannot fall arbitrarily far behind the screen?
- On session end, should the tail become editor input, or should it be discarded once the child has exited and only in-flight Ctrl+C during the command must be preserved?

The recommendation is to delay further reads, keep output moving, and return the tail to the editor when the child has exited.

## Decision

Pending.
