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
