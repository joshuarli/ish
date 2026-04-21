use crate::alias::AliasMap;
use epsh::ast::Word;
use epsh::lexer::{Lexer, Token};
use std::ffi::OsStr;
use std::path::PathBuf;

/// Load config from the given path, or the default config path.
/// Supports: `set VAR "value"` and `alias name cmd [args...]`.
/// Also sources `init.sh` from the config directory via epsh.
/// Warns on bad lines, continues processing.
pub fn load(aliases: &mut AliasMap, epsh: &mut epsh::eval::Shell, path_override: Option<&OsStr>) {
    let path = match path_override {
        Some(p) => PathBuf::from(p),
        None => config_path(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            // Only warn if the user explicitly specified a config file
            if path_override.is_some() {
                eprintln!("ish: {}: {e}", path.display());
            }
            return;
        }
    };

    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("set ") {
            parse_set(rest.trim(), lineno + 1, &path, epsh);
        } else if let Some(rest) = line.strip_prefix("alias ") {
            parse_alias(rest.trim(), lineno + 1, &path, aliases);
        } else {
            eprintln!(
                "ish: {}:{}: unrecognized directive: {line}",
                path.display(),
                lineno + 1,
            );
        }
    }

    // Source init.sh from the same config directory
    let init_path = path.with_file_name("init.sh");
    if init_path.exists() {
        match std::fs::read_to_string(&init_path) {
            Ok(script) => {
                let status = epsh.run_script(&script);
                if status != 0 {
                    eprintln!("ish: {}: exited with status {status}", init_path.display());
                }
            }
            Err(e) => {
                eprintln!("ish: {}: {e}", init_path.display());
            }
        }
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

    // Parse value_src through epsh's lexer to get a fully-typed Word, then
    // expand it (tilde, $VAR, ${VAR:-default}, $(cmd), etc.) via epsh.
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
            _ => value_src.to_string(),
        }
    };

    crate::shell_setenv(name, &expanded);
    // Sync to epsh's variable store (set + export so it's visible to subprocesses)
    let _ = epsh.vars_mut().set(name, &expanded);
    epsh.vars_mut().export(name);
}

fn parse_alias(rest: &str, lineno: usize, path: &std::path::Path, aliases: &mut AliasMap) {
    // alias name cmd [args...] — lex the whole line with epsh's lexer
    let mut lex = Lexer::new(rest);
    lex.recognize_reserved = false;
    let mut words: Vec<String> = Vec::new();
    loop {
        match lex.next_token() {
            Ok((Token::Word(parts, _), _)) => {
                words.push(epsh::lexer::parts_to_text(&parts));
            }
            Ok((Token::Eof, _)) | Err(_) => break,
            Ok(_) => break,
        }
    }

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

fn config_path() -> PathBuf {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("ish/config.ish")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/ish/config.ish")
    } else {
        PathBuf::from("/etc/ish/config.ish")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn config_path_uses_non_utf8_xdg_config_home() {
        let key = "XDG_CONFIG_HOME";
        let saved = std::env::var_os(key);
        let raw = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xf0, 0x80, 0x80, b'x']);

        crate::shell_setenv_os(key, &raw);
        let path = config_path();

        if let Some(saved) = saved {
            crate::shell_setenv_os(key, saved);
        } else {
            crate::shell_unsetenv(key);
        }

        assert_eq!(path, PathBuf::from(raw).join("ish/config.ish"));
    }
}
