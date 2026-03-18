use crate::alias::AliasMap;
use std::path::PathBuf;

/// Load config from the given path, or the default config path.
/// Supports: `set VAR "value"` and `alias name cmd [args...]`.
/// Warns on bad lines, continues processing.
pub fn load(aliases: &mut AliasMap, path_override: Option<&str>) {
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
            parse_set(rest.trim(), lineno + 1, &path);
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
}

fn parse_set(rest: &str, lineno: usize, path: &std::path::Path) {
    // set VAR "value" or set VAR value
    let (name, value) = match rest.split_once(char::is_whitespace) {
        Some((n, v)) => (n.trim(), unquote(v.trim())),
        None => (rest, String::new()),
    };

    if name.is_empty() {
        eprintln!(
            "ish: {}:{}: set: missing variable name",
            path.display(),
            lineno
        );
        return;
    }

    // Expand variables in value
    let expanded = expand_vars_simple(&value);
    // SAFETY: single-threaded shell, called during startup
    unsafe { std::env::set_var(name, &expanded) };
}

fn parse_alias(rest: &str, lineno: usize, path: &std::path::Path, aliases: &mut AliasMap) {
    // alias name cmd [args...]
    let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
    if parts.is_empty() {
        eprintln!("ish: {}:{}: alias: missing name", path.display(), lineno);
        return;
    }

    let name = parts[0].to_string();
    let expansion: Vec<String> = if parts.len() > 1 {
        shell_words(parts[1])
    } else {
        Vec::new()
    };

    if expansion.is_empty() {
        eprintln!(
            "ish: {}:{}: alias: missing expansion for '{name}'",
            path.display(),
            lineno,
        );
        return;
    }

    aliases.set(name, expansion);
}

/// Remove surrounding quotes from a value.
pub fn unquote(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Simple $VAR expansion for config values.
pub fn expand_vars_simple(s: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            i += 1;
            let mut name = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                name.push(chars[i]);
                i += 1;
            }
            if let Ok(val) = std::env::var(&name) {
                result.push_str(&val);
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Simple word splitting respecting quotes.
pub fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for c in s.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}

fn config_path() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("ish/config.ish")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/ish/config.ish")
    } else {
        PathBuf::from("/etc/ish/config.ish")
    }
}
