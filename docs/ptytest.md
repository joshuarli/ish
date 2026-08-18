# Interactive-shell PTY tests

`tests/pty.rs` keeps a small `PtyShell` adapter for shell vocabulary, fixtures,
prompt barriers, and filesystem test setup. `ptytest` owns PTY spawn, polling,
terminal parsing, process groups, and cleanup. Run the focused suite with
`cargo test --test pty`.

The shell scenarios use a validated `C.UTF-8` hermetic environment and the
audited `xterm-minimal-v1` profile, including ish's OSC 7 working-directory
metadata. Synchronize with a prompt, cursor, screen, or other shell event; do
not add a generic settle sleep. The only documented waits are filesystem
timestamp-boundary checks for `denv` trust invalidation.

Normal `exit` and Ctrl-D scenarios also compare the final represented terminal
lifecycle state with the state captured at spawn. Forced process cleanup is a
safety net, not a restoration assertion.

Failure bundles are written below `target/ptytest-failures/`. Any future
snapshot is stored beside its scenario and updated only with
`PTYTEST_UPDATE_SNAPSHOTS=1`.
