# Runtime architecture

epsh parses shell source into a typed AST, expands words during evaluation, and
runs builtins, functions, or external commands. The CLI in [main.rs](../src/main.rs)
uses the same library API as embedders.

The module declarations in [lib.rs](../src/lib.rs) expose the parser, evaluator,
expansion, builtin, byte, and OS layers.

The lexer constructs typed word parts in one pass so quoting context remains
available to expansion. The recursive-descent parser builds commands with
source spans and reads queued here-documents after the command line is complete.
The evaluator owns variables, functions, options, traps, and a working directory
per Shell instance.

Several evaluator rules are easy to break during changes:

- Errexit is checked at simple commands, pipelines, and subshells. Tested
  contexts such as conditions and the left side of AND-OR lists suppress it.
- Pipeline children execute their stage directly when already forked. Nested
  subshell syntax in that child must not create a second child process.

The builder and `Shell` expose an optional `CommandPolicy`. epsh checks the
expanded command target before builtin or function dispatch, including nested
`command` indirection and command substitutions. The default has no policy.
An embedder can return a diagnostic to stop dispatch with status 2. The policy
does not undo earlier command or expansion effects.

`Shell::set_command_placement` adds an owned command name to an opt-in
placement table. A `ParentProcess` target is checked before command-policy
callbacks and target redirections. epsh refuses it with status 2 in a forked
process, pipeline, or command substitution. The table is empty by default.

The [arithmetic contract](../README.md#builder-options) defines wrapping,
errors, shift behavior, and assignment effects.

Use shell terms consistently: a token is a lexical unit; a word is an
unexpanded sequence of word parts; a field is an argument after expansion. A
pipeline joins commands with a pipe. An AND-OR list joins commands with && or
||; a sequential list joins commands with ; or a newline. List is the umbrella
term. Parameter expansion handles values such as HOME; pathname expansion
handles patterns such as *.rs.

The parser can accept constructs beyond the contract an embedding application
chooses to expose. The embedding API and its tests define that contract.
See [byte preservation](byte-preservation.md) for the runtime data model and
[process handling](process-model.md) for execution boundaries.
