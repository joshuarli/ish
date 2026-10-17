# Runtime architecture

epsh parses shell source into a typed AST, expands words during evaluation, and
runs builtins, functions, or external commands. The CLI in [main.rs](../src/main.rs)
uses the same library API as embedders.

| Area | Source |
|------|--------|
| Tokens, words, AST, and parsing | [lexer.rs](../src/lexer.rs), [parser.rs](../src/parser.rs), [ast.rs](../src/ast.rs) |
| Shell state and command evaluation | [eval.rs](../src/eval.rs), [var.rs](../src/var.rs) |
| Parameter, arithmetic, and pathname expansion | [expand.rs](../src/expand.rs), [arith.rs](../src/arith.rs), [glob.rs](../src/glob.rs) |
| Builtins and test expressions | [builtins.rs](../src/builtins.rs), [test_cmd.rs](../src/test_cmd.rs) |
| Byte and OS boundaries | [shell_bytes.rs](../src/shell_bytes.rs), [encoding.rs](../src/encoding.rs), [redirect.rs](../src/redirect.rs), [sys.rs](../src/sys.rs) |
| Errors and signal traps | [error.rs](../src/error.rs), [signal.rs](../src/signal.rs) |

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
- Eligible simple builtin command substitutions and the file-only input
  redirection command substitution have in-process paths.

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
