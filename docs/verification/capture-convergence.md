# Capture convergence regression record

Fullscreen programs (`less`, `git log`) wedged under output capture: the
split relay never forwarded typed input to the capture terminal, so keyboard
readers waited forever. Bisected to the capture introduction (commit
`2041fba`); pre-capture `7ac90c8` runs `less` fine, capture-era `729b982`
wedges it. A program-name bypass was rejected as the durable fix in favor of
the structural rule in `dispatch::is_single_simple_command`: single simple
commands converge stdin/stdout/stderr onto the slave and claim it as their
controlling terminal; pipelines and lists keep split capture.

Five root causes found while converging, each with a lock:

1. Stale terminal descriptor. epsh cached a dup of fd 0, but converged
   capture swaps fd 0 every command; the second spawn inherited a dead
   descriptor (`TIOCSCTTY: ENOENT`, then `ENOTTY`/`EPERM` for pipelines).
   Fix: `set_foreground_ctty` clears the cache when arming/disarming.
2. Orphaned-group suspend discarded. Converged children live in a session
   of their own, and macOS discards terminal-generated `SIGTSTP` for
   orphaned groups (confirmed against `script(1)`; direct `killpg(TSTP)`
   also ignored, `STOP` honored). Fix: relay consumes `VSUSP` and sends
   `SIGSTOP` to the slave foreground group. Lock: `job_suspend_and_resume`
   plus stop/resume suites.
3. Unconditional `SIGWINCH` feedback loop. Signalling on every 200 ms poll
   made `less` redraw continuously (19 MB in 4 s, 70k screens), starving
   `q` and tripping the harness OSC limit. Fix: signal only on actual size
   change. Lock: strict resize test (exactly one `12 50` report, no `24 80`).
4. Lost keyboard signals for in-process builtins. Native `cp` owns no
   foreground group, so forwarded `Ctrl-C` sat queued and the copy
   completed with status 0. Fix: relay signals the shell's group for
   `VINTR`/`VQUIT` when no child owns the slave; suspend stays ignored
   there. Lock: `native_fs_ctrl_c_during_copy_returns_130`,
   `native_fs_ctrl_z_is_ignored_during_copy`.
5. `tcgetpgrp` on macOS slave fds fails (`ENOTTY`) while the master works,
   and pgid 0 (no foreground) trips a rustix assertion. Fix:
   `foreground_group` reads via raw libc through the master, mapping
   `<= 0` to none. Lock: suspend/resize/pager suites on macOS.

Related hardening: stderr falls back to `/dev/tty` when it points at a dead
capture slave so spawn diagnostics stay visible; relay panics are reported
instead of hanging `finish_into`; `fg` restores raw real-terminal modes and
slave modes separately for converged jobs.

Harness note: `run_command` may report a fast command twice (bytes arriving
with the line advance are stashed for the next read and re-read by the
prompt wait). Count-sensitive tests must assert set properties
(`single_command_sees_one_terminal` requires ≥ 3 identical tty paths), not
exact multiplicities.
