# PTY tests

[`tests/pty.rs`](../tests/pty.rs) adapts the shell vocabulary and fixtures to
the `laputa-ptytest` PTY harness. Run the focused suite with
`cargo test --test pty`.

The harness uses a hermetic ASCII environment on Linux and `C.UTF-8` elsewhere,
with the `xterm-minimal-v1` terminal profile. Tests synchronize on prompts,
cursors, screens, and other shell events. Short bounded quiescence waits are
used for terminal transitions; a few filesystem checks wait for timestamp
boundaries when testing denv trust invalidation. Avoid arbitrary settle sleeps.

Normal exit and Ctrl+D tests verify that the represented terminal lifecycle is
restored to its initial state. Cleanup after a failed test only prevents a
stray process; it is not a restoration assertion.

The panic hook in [term.rs](../src/term.rs) attempts terminal restoration before
calling the previous hook. Release builds abort on panic, so cleanup cannot
rely on `RawMode::drop`. Development builds use the default unwind strategy.

## Native file operation cases

Name `cp` and `mv` PTY cases with the `native_fs_` prefix. Cover list
continuation, output redirection, and argument substitution in the parent
process. Check pipeline and command-substitution refusals before target
redirections or filesystem effects. Include aliases, expanded command names,
and nested `command` dispatch.

For cancellation cases, start work against an owned fixture, send Ctrl+C, and
wait for both status 130 and the next usable prompt. Inspect source and
destination entries after interrupted moves. Ctrl+Z must not suspend an
in-process file operation; retain the existing suspend/resume checks for
external foreground commands. Verify terminal restoration through the PTY
state, not only process cleanup.

The `native_fs_*` lifecycle cases exercise parent-process dispatch, redirection,
argument expansion, cancellation, terminal restoration, and external job
control. They pass on Linux. This PTY evidence does not establish native
macOS behavior, filesystem clone support, cross-device retirement, or
performance.
