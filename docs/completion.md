# Completion

Tab completion chooses command names, builtins, aliases, SSH hosts, or filesystem
paths from the current input. A blank input starts `cd`; after `cd`, only
directories are offered. SSH host names come from `~/.ssh/config` and
`~/.ssh/known_hosts`.

Candidates match the word text before the cursor. Accepting a candidate
replaces the whole word around the cursor, including any closing quote, and
leaves later arguments unchanged. An accepted completion is one undo step; a
cancelled one adds none.

Filesystem candidates use case-sensitive prefix matches first. Case-insensitive
substring matches are offered only when there are no prefix matches. Hidden entries are
omitted unless the typed prefix begins with `.`, and directory-only contexts
exclude other entries. Path candidates are sorted by modification time, newest
first, with case-insensitive name order for ties. Duplicate names are removed.

Partial path components can resolve to more than one directory. Each
component expands to at most 64 directories, the first in path order; matches
beyond that bound are dropped while the directory is read, so nested broad
prefixes cannot multiply traversal. Completion stays in-process. Entry metadata uses
`fstatat` against an opened directory, and candidate buffers are reused.

The implementation and ordering tests are in
[`src/complete.rs`](../src/complete.rs). Input handling and the completion UI
are in [`src/editor.rs`](../src/editor.rs) and [`src/render.rs`](../src/render.rs);
shell-level coverage is in [`tests/integration.rs`](../tests/integration.rs)
and the [PTY tests](../tests/pty.rs).
