# Byte preservation

Unix shell values are byte sequences. [ShellBytes](../src/shell_bytes.rs) stores
them as Vec<u8> and converts to OsStr, Path, and CString at operating-system
boundaries. Variable values, positional parameters, word literals, and
here-document bodies use this representation.

Some parser and expansion paths still use Rust String. The compatibility
encoding in [encoding.rs](../src/encoding.rs) maps invalid UTF-8 bytes to
Unicode private-use characters in those strings and restores the original
bytes when converting back. This encoding is an internal bridge, not a display
format.

The byte-preserving paths include:

- CLI arguments and script paths, imported with args_os.
- Environment entries, imported with vars_os. Entries whose names are not
  shell identifiers remain available to child environments.
- Exported variables and command-prefix assignments passed to children.
- External command arguments and direct exec arguments.
- Redirection paths, path arguments to builtins, and glob directory entries.
- Captured command-substitution output.

The child environment merges inherited entries, exported shell variables, and
command-prefix assignments. Exported variables and prefix assignments shadow
inherited entries with the same name; a prefix assignment takes precedence for
that command. env_clear starts without inherited entries.

Use set_var_bytes and get_var_bytes for arbitrary variable values. The string
convenience getter returns None for values that are not valid UTF-8. Converting
to a C string can fail when the data contains NUL.

Byte-preservation coverage lives in the raw_bytes module of
[integration.rs](../tests/integration.rs) and the byte-oriented embedding
checks in [embedding.rs](../tests/embedding.rs). Tests for invalid path bytes
skip when the host filesystem does not accept them; argv and environment
round-trip checks do not depend on such filesystem support.
