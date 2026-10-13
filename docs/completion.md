# Completion

Tab completion chooses command names, builtins, aliases, SSH hosts, or filesystem
paths from the current input. A blank input starts `cd`; after `cd`, only
directories are offered. SSH host names come from `~/.ssh/config` and
`~/.ssh/known_hosts`.

Filesystem candidates use case-insensitive prefix matches first. Substring
matches are offered only when there are no prefix matches. Hidden entries are
omitted unless the typed prefix begins with `.`, and directory-only contexts
exclude other entries. Path candidates are sorted by modification time, newest
first, with case-insensitive name order for ties. Duplicate names are removed.

Partial path components can resolve to more than one directory; expansion is
capped at 64 directories. Completion reads one directory at a time and does not
start a subprocess. The per-keystroke path uses reusable candidate storage, so
keep the warm path allocation-conscious.

The implementation and ordering tests are in
[`src/complete.rs`](../src/complete.rs). Input handling and the completion UI
are in [`src/main.rs`](../src/main.rs) and [`src/render.rs`](../src/render.rs);
shell-level coverage is in [`tests/integration.rs`](../tests/integration.rs)
and the [PTY tests](../tests/pty.rs).
