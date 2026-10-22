use crate::alias::AliasMap;
use crate::complete::needs_quoting;
use crate::static_word::static_word;
use crate::{builtin, path};
use std::path::Path;

pub fn maybe_rewrite_cd(line: &str, aliases: &AliasMap, home: &std::ffi::OsStr, path_env: &[u8]) -> String {
    let trimmed = line.trim();
    let words = match lex_simple_command_words(trimmed) {
        Some(words) => words,
        None => return line.to_string(),
    };
    let first = match words.first() {
        Some(word) => word,
        None => return line.to_string(),
    };
    let first_bytes = epsh::shell_bytes::ShellBytes::from_str_lossless(&epsh::lexer::parts_to_text(&first.parts));
    let Some(first_text) = first_bytes.as_utf8_str() else {
        return line.to_string();
    };
    let first_quoted = epsh::lexer::parts_have_quoting(&first.parts);

    if !first_quoted && first_text == "cd" {
        return line.to_string();
    }

    if words.len() == 1
        && !first_quoted
        && first_text.len() >= 2
        && first_text.bytes().all(|b| b == b'.')
    {
        let levels = first_text.len() - 1;
        let path = (0..levels).map(|_| "..").collect::<Vec<_>>().join("/");
        return format!("cd {path}");
    }

    if words.len() != 1 || builtin::is_builtin(first_text) || builtin::is_refused_epsh_builtin(first_text) || aliases.get(first_text).is_some() {
        return line.to_string();
    }

    let expanded = first_text.strip_prefix('~').map(|rest| {
        let mut path = home.to_os_string();
        path.push(rest);
        std::path::PathBuf::from(path)
    });
    let path = expanded.as_deref().unwrap_or_else(|| Path::new(first_text));
    if path.is_dir()
        && !is_executable(path)
        && (first_text.contains('/') || path::scan_path(first_text, path_env).is_none())
    {
        return format!("cd {trimmed}");
    }

    line.to_string()
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0 && m.is_file())
        .unwrap_or(false)
}

pub(crate) fn lex_simple_command_words(line: &str) -> Option<Vec<epsh::ast::Word>> {
    let mut lex = epsh::lexer::Lexer::new(line);
    lex.recognize_reserved = false;

    let mut words = Vec::new();
    loop {
        match lex.next_token() {
            Ok((epsh::lexer::Token::Word(parts, _), span)) => {
                words.push(epsh::ast::Word { parts, span });
            }
            Ok((epsh::lexer::Token::Eof, _)) => break,
            Ok(_) | Err(_) => return None,
        }
    }

    (!words.is_empty()).then_some(words)
}

pub fn resolve_cd_for_history(line: &str) -> String {
    use epsh::lexer::{Lexer, Token};

    let trimmed = line.trim();
    let mut lexer = Lexer::new(trimmed);
    lexer.recognize_reserved = false;
    let Ok((Token::Word(command_parts, _), _)) = lexer.next_token() else {
        return line.to_string();
    };
    let Some(command) = static_word(&command_parts) else {
        return line.to_string();
    };
    let prefix = match command.as_str() {
        "cd" => "cd ",
        "z" => "z ",
        _ => return line.to_string(),
    };
    let Ok((Token::Word(parts, _), _)) = lexer.next_token() else {
        return line.to_string();
    };
    let Some(text) = static_word(&parts) else {
        return line.to_string();
    };
    if text.is_empty() || text.starts_with(['/', '~']) || text == "-" || text.contains(['*', '?', '[']) {
        return line.to_string();
    }
    let Ok((_, next_span)) = lexer.next_token() else {
        return line.to_string();
    };
    let suffix_start = trimmed
        .char_indices()
        .nth(next_span.offset)
        .map_or(trimmed.len(), |(index, _)| index);
    let suffix = &trimmed[suffix_start..];
    let Ok(cwd) = std::env::current_dir() else {
        return line.to_string();
    };
    let target = epsh::shell_bytes::ShellBytes::from_str_lossless(&text).to_path_buf();
    let resolved = cwd.join(target);
    let resolved = resolved.canonicalize().unwrap_or(resolved);
    let Some(path) = resolved.to_str() else {
        return line.to_string();
    };
    let target = if needs_quoting(path) || path.contains('\'') || path.chars().any(char::is_control) {
        format!("'{}'", path.replace('\'', "'\\''"))
    } else {
        path.to_string()
    };
    if suffix.is_empty() {
        format!("{prefix}{target}")
    } else {
        format!("{prefix}{target} {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_cd_for_history;

    #[test]
    fn tab_separated_cd_history_target_is_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let expected = cwd.join("child");

        assert_eq!(
            resolve_cd_for_history("cd\tchild && pwd"),
            format!("cd {} && pwd", expected.display())
        );
        assert_eq!(
            resolve_cd_for_history("z\tchild"),
            format!("z {}", expected.display())
        );

        let unicode_expected = cwd.join("子");
        assert_eq!(
            resolve_cd_for_history("cd\t子 && pwd"),
            format!("cd {} && pwd", unicode_expected.display())
        );
    }
}
