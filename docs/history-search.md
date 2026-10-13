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
interactive search UI is in [`src/main.rs`](../src/main.rs). Keep the ranking
and snapshot behavior covered by the focused history tests and the shell
integration and [PTY tests](../tests/pty.rs) when changing it.
