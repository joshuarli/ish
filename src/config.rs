use crate::alias::AliasMap;
use epsh::ast::Word;
use epsh::lexer::{Lexer, Token};
use std::ffi::OsStr;
use std::path::PathBuf;

/// Load config from the given path, or the default config path.
/// Supports: `set VAR "value"` and `alias name cmd [args...]`.
/// Reports every bad line and keeps processing the rest.
pub fn load(aliases: &mut AliasMap, epsh: &mut epsh::eval::Shell, path_override: Option<&OsStr>) {
    load_with_home(
        aliases,
        epsh,
        path_override,
        std::env::var_os("HOME").as_deref(),
    );
}

/// Load config using an explicit HOME value from the shell's variable store.
pub fn load_with_home(
    aliases: &mut AliasMap,
    epsh: &mut epsh::eval::Shell,
    path_override: Option<&OsStr>,
    home: Option<&OsStr>,
) {
    let path = match path_override {
        Some(p) => PathBuf::from(p),
        None => config_path_for_home(home),
    };
    let content = match std::fs::read(&path) {
        Ok(c) => c,
        // A missing default config is normal; a missing explicit one is not.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && path_override.is_none() => return,
        Err(e) => {
            eprintln!("ish: {}: {e}", path.display());
            return;
        }
    };

    // Lines are decoded one at a time so a stray invalid byte costs only its
    // own line, not the valid directives around it.
    for (lineno, raw) in content.split(|&b| b == b'\n').enumerate() {
        let lineno = lineno + 1;
        let line = match std::str::from_utf8(raw) {
            Ok(line) => line.trim(),
            Err(_) => {
                eprintln!("ish: {}:{lineno}: line is not valid UTF-8", path.display());
                continue;
            }
        };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (directive, rest) = match line.split_once(char::is_whitespace) {
            Some((directive, rest)) => (directive, rest.trim()),
            None => (line, ""),
        };
        match directive {
            "set" => parse_set(rest, lineno, &path, epsh),
            "alias" => parse_alias(rest, lineno, &path, aliases),
            _ => eprintln!(
                "ish: {}:{lineno}: unrecognized directive: {line}",
                path.display(),
            ),
        }
    }

    // An init.sh beside the config used to be run through epsh. Running
    // arbitrary scripts is outside what ish does, so say so instead of
    // silently ignoring a file the user expects to take effect.
    let init_path = path.with_file_name("init.sh");
    if init_path.exists() {
        eprintln!(
            "ish: {}: init.sh is not run; move its settings into config.ish",
            init_path.display()
        );
    }
}

fn parse_set(rest: &str, lineno: usize, path: &std::path::Path, epsh: &mut epsh::eval::Shell) {
    // set VAR "value with $EXPANSION" — lex the rest to get name then value word
    let (name, value_src) = match rest.split_once(char::is_whitespace) {
        Some((n, v)) => (n.trim(), v.trim()),
        None => (rest, ""),
    };

    if name.is_empty() {
        eprintln!(
            "ish: {}:{}: set: missing variable name",
            path.display(),
            lineno
        );
        return;
    }

    let valid_name = name
        .bytes()
        .enumerate()
        .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()));
    if !valid_name {
        eprintln!(
            "ish: {}:{}: set: invalid variable name '{name}'",
            path.display(),
            lineno
        );
        return;
    }

    // Parse value_src through epsh's lexer to get a fully-typed Word, then
    // expand it (tilde, $VAR, $(cmd), etc.) via epsh.
    let expanded = if value_src.is_empty() {
        String::new()
    } else {
        let mut lex = Lexer::new(value_src);
        // Disable reserved-word recognition — we just want word tokens.
        lex.recognize_reserved = false;
        match lex.next_token() {
            Ok((Token::Word(parts, _), span)) => {
                let word = Word { parts, span };
                match epsh::expand::expand_word_to_string(&word, epsh) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!(
                            "ish: {}:{}: set {name}: expansion error: {e}",
                            path.display(),
                            lineno
                        );
                        return;
                    }
                }
            }
            Ok(_) => {
                eprintln!(
                    "ish: {}:{}: set {name}: value must start with a word",
                    path.display(),
                    lineno
                );
                return;
            }
            Err(e) => {
                eprintln!("ish: {}:{}: set {name}: {e}", path.display(), lineno);
                return;
            }
        }
    };

    // Config values belong to epsh's store; child processes receive them through
    // the normal external-handler environment path.
    if let Err(e) = epsh.vars_mut().set(name, &expanded) {
        eprintln!("ish: {}:{}: set {name}: {e}", path.display(), lineno);
        return;
    }
    epsh.vars_mut().export(name);
}

fn parse_alias(rest: &str, lineno: usize, path: &std::path::Path, aliases: &mut AliasMap) {
    let mut words = crate::alias::lex_words(rest);

    if words.is_empty() {
        eprintln!("ish: {}:{}: alias: missing name", path.display(), lineno);
        return;
    }

    let name = words.remove(0);
    if words.is_empty() {
        eprintln!(
            "ish: {}:{}: alias: missing expansion for '{name}'",
            path.display(),
            lineno,
        );
        return;
    }

    aliases.set(name, words);
}

fn config_path_for_home(home: Option<&OsStr>) -> PathBuf {
    if let Some(home) = home {
        PathBuf::from(home).join(".config/ish/config.ish")
    } else {
        PathBuf::from("/etc/ish/config.ish")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load `content` as a config file. Files live under the worktree's
    /// scratch directory, one per test name.
    fn load_text(name: &str, content: &[u8]) -> (AliasMap, epsh::eval::Shell) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("config-test-{name}"));
        std::fs::write(&path, content).unwrap();
        let mut aliases = AliasMap::new();
        let mut shell = epsh::eval::Shell::new();
        load_with_home(&mut aliases, &mut shell, Some(path.as_os_str()), None);
        std::fs::remove_file(&path).unwrap();
        (aliases, shell)
    }

    #[test]
    fn invalid_utf8_costs_only_its_own_line() {
        let (aliases, shell) = load_text("utf8", b"alias a echo one\nset BAD \xff\n\xff\nalias b echo two\n");
        assert!(aliases.get("a").is_some() && aliases.get("b").is_some());
        assert_eq!(shell.vars().get("BAD"), None);
    }

    #[test]
    fn bad_set_lines_do_not_define_variables() {
        let (_, shell) = load_text("badset", b"set 1BAD x\nset FOO \"unterminated\nset GOOD ok\n");
        assert_eq!(shell.vars().get("1BAD"), None);
        assert_eq!(shell.vars().get("FOO"), None);
        assert_eq!(shell.vars().get("GOOD"), Some("ok"));
    }
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn config_path_uses_non_utf8_home() {
        let raw = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xf0, 0x80, 0x80, b'x']);
        let path = config_path_for_home(Some(raw.as_os_str()));
        assert_eq!(path, PathBuf::from(raw).join(".config/ish/config.ish"));
    }
}
