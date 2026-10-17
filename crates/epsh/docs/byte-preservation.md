# Byte preservation

Unix shell values are byte sequences. [ShellBytes](../src/shell_bytes.rs) stores
them as `Vec<u8>` and converts them to `OsStr`, `Path`, and `CString` at
operating-system boundaries. Variable values, positional parameters, word
literals, and here-document bodies use this representation.

Public text inputs preserve their UTF-8 bytes. `Parser::new` and
`Lexer::new` accept Rust strings; `Parser::new_bytes` and `Lexer::new_bytes`
accept arbitrary bytes. `Shell::set_var` stores the UTF-8 bytes of its string
argument. Use `set_var_bytes` when a variable value contains bytes that are not
valid UTF-8.

Some parser and expansion paths use Rust `String` internally. The codec in
[encoding.rs](../src/encoding.rs) carries arbitrary bytes through those paths.
It maps invalid UTF-8 bytes to private-use markers and prefixes literal marker
characters so they remain distinct. The reverse conversion restores the
original byte sequence, including invalid UTF-8. This encoding is private to
the implementation and is not a display format.

The byte-preserving paths include:

- CLI arguments and script paths, imported with `args_os`.
- Environment entries, imported with `vars_os`. Entries whose names are not
  shell identifiers remain available to child environments.
- Exported variables and command-prefix assignments passed to children.
- External command arguments and direct exec arguments.
- Redirection paths, path arguments to builtins, and glob directory entries.
- Captured command-substitution output.

The child environment merges inherited entries, exported shell variables, and
command-prefix assignments. Exported variables and prefix assignments shadow
inherited entries with the same name. A prefix assignment takes precedence for
that command. `env_clear` starts without inherited entries.

Use `set_var_bytes` and `get_var_bytes` for arbitrary variable values. The
string convenience getter returns `None` for values that are not valid UTF-8.
Converting bytes to a C string can fail when the data contains NUL.

Byte-preservation coverage lives in the `raw_bytes` module of
[integration.rs](../tests/integration.rs), the byte-oriented embedding checks
in [embedding.rs](../tests/embedding.rs), and
[byte_codec.rs](../tests/byte_codec.rs). Tests for invalid path bytes skip when
the host filesystem does not accept them. Argument and environment round-trip
checks do not depend on invalid path support.
