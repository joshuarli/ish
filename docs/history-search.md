# History search

Ctrl+R searches saved commands and highlights matching characters. Ranking is
literal-first:

1. Case-insensitive prefix matches.
2. Case-insensitive substrings at a word boundary.
3. Other case-insensitive substrings.
4. Case-insensitive subsequence matches.

Directory proximity, usage frequency, and recency refine results within a text
match tier; they do not promote a weaker text match over a stronger one. A
blank query shows recent commands. Matching positions are character-based so
Unicode text can be highlighted correctly.

Ctrl+R synchronizes saved history when search opens, including commands written
by other running shells. Up and Down history navigation and autosuggestions use
a session snapshot plus commands entered in the current session.

The ranking implementation is in [`src/history.rs`](../src/history.rs); the
interactive search UI is in [`src/editor.rs`](../src/editor.rs). Keep the ranking
and snapshot behavior covered by the focused history tests and the shell
integration and [PTY tests](../tests/pty.rs) when changing it.

ASCII byte scanning and subsequence eligibility come from the vendored fz
library. The production path borrows history's text arena. History owns its
literal tiers, contextual ranking, Unicode case matching and character-index
highlights, including commands beyond fz's 1024-byte scoring boundary.
The complete eligible set remains available when narrowing beyond 200 visible
rows. Query edits retain the existing append, backspace and arbitrary-edit
cache rules. See [shared-source updates](fuzzy-pickers.md#shared-source).
