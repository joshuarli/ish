use std::borrow::Cow;

use crate::error::Error;
/// Re-exported from parse. See [`parse::LITERAL`] for the protocol docs.
/// `\x00` before a char means "this char is literal, do not expand it".
/// Each expansion stage (variables, command subst, glob) checks for this
/// marker and skips marked characters. `strip_literal()` removes all
/// markers after expansion is complete.
use crate::parse::LITERAL;

/// Full expansion pipeline for a single word. Returns one or more strings
/// (glob can expand to multiple). The `exec_subst` callback runs command
/// substitutions and returns their stdout.
pub fn expand_word(
    word: &str,
    home: &str,
    exec_subst: &mut dyn FnMut(&str) -> Result<String, Error>,
) -> Result<Vec<String>, Error> {
    // Avoid allocations when no expansion is needed.
    let has_dollar = word.contains('$') || word.contains('`');
    let has_tilde = word.starts_with('~');
    let has_glob = has_glob_chars(word);

    // Fast path: no expansion at all — zero allocation
    if !has_dollar && !has_tilde && !has_glob && !word.contains(LITERAL) {
        return Ok(vec![word.to_string()]);
    }

    // Use Cow to avoid allocation when stages are skipped.
    let word: Cow<str> = if has_tilde {
        Cow::Owned(expand_tilde(word, home))
    } else {
        Cow::Borrowed(word)
    };
    let word: Cow<str> = if has_dollar {
        let w = expand_variables(&word);
        Cow::Owned(expand_command_subst(&w, exec_subst)?)
    } else {
        word
    };
    if has_glob {
        expand_glob(&word)
    } else {
        Ok(vec![strip_literal(&word).into_owned()])
    }
}

/// Expand argv in place. Each word may produce multiple results (from globs).
pub fn expand_argv(
    argv: &[String],
    home: &str,
    exec_subst: &mut dyn FnMut(&str) -> Result<String, Error>,
) -> Result<Vec<String>, Error> {
    let mut result = Vec::new();
    for word in argv {
        result.extend(expand_word(word, home, exec_subst)?);
    }
    Ok(result)
}

// -- Tilde Expansion --

fn expand_tilde(word: &str, home: &str) -> String {
    if word == "~" {
        return home.to_string();
    }
    if let Some(rest) = word.strip_prefix("~/") {
        let mut s = String::with_capacity(home.len() + 1 + rest.len());
        s.push_str(home);
        s.push('/');
        s.push_str(rest);
        return s;
    }
    word.to_string()
}

// -- Variable Expansion --

fn expand_variables(word: &str) -> String {
    let bytes = word.as_bytes();
    let mut result = String::with_capacity(word.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0 {
            // LITERAL marker — pass through with marker + next char
            result.push(LITERAL);
            i += 1;
            if i < bytes.len() {
                let b = bytes[i];
                if b < 0x80 {
                    // ASCII fast path
                    result.push(b as char);
                    i += 1;
                } else {
                    let rest = &word[i..];
                    let ch = rest.chars().next().unwrap();
                    result.push(ch);
                    i += ch.len_utf8();
                }
            }
        } else if bytes[i] == b'$' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'(' {
                // Command substitution — leave for later pass
                result.push('$');
                i += 1;
            } else {
                // Variable name: ASCII alphanumeric + underscore
                i += 1;
                let name_start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if i == name_start {
                    result.push('$');
                } else {
                    let name = &word[name_start..i];
                    if let Ok(val) = std::env::var(name) {
                        result.push_str(&val);
                    }
                }
            }
        } else {
            // Copy non-special content in bulk
            let start = i;
            while i < bytes.len() && bytes[i] != 0 && bytes[i] != b'$' {
                i += 1;
            }
            result.push_str(&word[start..i]);
        }
    }

    result
}

// -- Command Substitution --

fn expand_command_subst(
    word: &str,
    exec_subst: &mut dyn FnMut(&str) -> Result<String, Error>,
) -> Result<String, Error> {
    let bytes = word.as_bytes();
    let mut result = String::with_capacity(word.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0 {
            // LITERAL marker
            result.push(LITERAL);
            i += 1;
            if i < bytes.len() {
                let b = bytes[i];
                if b < 0x80 {
                    // ASCII fast path
                    result.push(b as char);
                    i += 1;
                } else {
                    let rest = &word[i..];
                    let ch = rest.chars().next().unwrap();
                    result.push(ch);
                    i += ch.len_utf8();
                }
            }
        } else if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            // $(...) command substitution
            let start = i + 2;
            let end = find_matching_paren(bytes, start)?;
            let cmd = strip_literal(&word[start..end]);
            let output = exec_subst(&cmd)?;
            result.push_str(output.trim_end_matches('\n'));
            i = end + 1;
        } else if bytes[i] == b'`' {
            // Backtick command substitution
            let start = i + 1;
            let end = find_backtick_end(bytes, start)?;
            let cmd = strip_literal(&word[start..end]);
            let output = exec_subst(&cmd)?;
            result.push_str(output.trim_end_matches('\n'));
            i = end + 1;
        } else if bytes[i] == b'$' {
            // Bare $ (not followed by '(') — pass through
            result.push('$');
            i += 1;
        } else {
            // Copy non-special content in bulk
            let start = i;
            while i < bytes.len() && bytes[i] != 0 && bytes[i] != b'$' && bytes[i] != b'`' {
                i += 1;
            }
            result.push_str(&word[start..i]);
        }
    }

    Ok(result)
}

fn find_matching_paren(bytes: &[u8], start: usize) -> Result<usize, Error> {
    let mut depth: u32 = 1;
    let mut i = start;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            0 => i += 2, // LITERAL + next byte (skip escaped char)
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    Err(Error::bad_substitution("unclosed $("))
}

fn find_backtick_end(bytes: &[u8], start: usize) -> Result<usize, Error> {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            0 => i += 2,
            b'`' => return Ok(i),
            _ => i += 1,
        }
    }
    Err(Error::bad_substitution("unclosed backtick"))
}

// -- Glob Expansion --

fn expand_glob(word: &str) -> Result<Vec<String>, Error> {
    // Check if word has any unescaped glob chars
    if !has_glob_chars(word) {
        // No glob — just strip LITERAL markers and return
        return Ok(vec![strip_literal(word).into_owned()]);
    }

    let pattern = strip_literal(word);
    let matches = glob_match(&pattern)?;

    if matches.is_empty() {
        return Err(Error::glob_no_match(&*pattern));
    }

    Ok(matches)
}

fn has_glob_chars(word: &str) -> bool {
    let bytes = word.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0 => i += 2, // LITERAL + next byte
            b'*' | b'?' => return true,
            _ => i += 1,
        }
    }
    false
}

fn strip_literal(s: &str) -> Cow<'_, str> {
    if !s.contains(LITERAL) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace(LITERAL, ""))
}

/// Expand a glob pattern into matching paths.
fn glob_match(pattern: &str) -> Result<Vec<String>, Error> {
    // Split pattern into path segments
    let segments: Vec<&str> = pattern.split('/').collect();

    let (base, seg_start) = if pattern.starts_with('/') {
        ("/".to_string(), 1)
    } else {
        (".".to_string(), 0)
    };

    let mut results = vec![base];

    for (idx, seg) in segments[seg_start..].iter().enumerate() {
        if seg.is_empty() {
            continue;
        }

        let is_last = idx == segments.len() - seg_start - 1;

        if *seg == "**" {
            // Recursive: expand ** to all descendant directories
            let mut new_results = Vec::new();
            for dir in &results {
                collect_recursive(dir, &mut new_results)?;
            }
            results = new_results;
        } else {
            // Match this segment against directory entries
            let mut new_results = Vec::new();
            for dir in &results {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        // Skip filenames with control characters
                        if name.bytes().any(|b| b < b' ' || b == 0x7f) {
                            continue;
                        }
                        // Skip hidden files unless pattern starts with .
                        if name.starts_with('.') && !seg.starts_with('.') {
                            continue;
                        }
                        if pattern_match(seg, &name) {
                            let path = if dir == "." {
                                name.to_string()
                            } else if dir == "/" {
                                format!("/{name}")
                            } else {
                                format!("{dir}/{name}")
                            };
                            // If not last segment, only keep directories.
                            // Use std::fs::metadata (stat, follows symlinks)
                            // instead of entry.metadata (lstat) so that
                            // symlinks to directories like /var → /private/var
                            // are traversed.
                            if !is_last {
                                if std::fs::metadata(&path)
                                    .map(|m| m.is_dir())
                                    .unwrap_or(false)
                                {
                                    new_results.push(path);
                                }
                            } else {
                                new_results.push(path);
                            }
                        }
                    }
                }
            }
            results = new_results;
        }
    }

    results.sort();
    Ok(results)
}

/// Recursively collect all directories from `dir` (including `dir` itself).
fn collect_recursive(dir: &str, out: &mut Vec<String>) -> Result<(), Error> {
    out.push(dir.to_string());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || name.bytes().any(|b| b < b' ' || b == 0x7f) {
                    continue;
                }
                let child = if dir == "." {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                };
                collect_recursive(&child, out)?;
            }
        }
    }
    Ok(())
}

/// Simple glob pattern matching: * matches any chars, ? matches one char.
/// Operates on bytes for speed (filenames are almost always ASCII).
fn pattern_match(pattern: &str, name: &str) -> bool {
    pattern_match_bytes(pattern.as_bytes(), name.as_bytes(), 0, 0)
}

fn pattern_match_bytes(p: &[u8], n: &[u8], pi: usize, ni: usize) -> bool {
    if pi == p.len() {
        return ni == n.len();
    }
    if p[pi] == b'*' {
        for k in ni..=n.len() {
            if pattern_match_bytes(p, n, pi + 1, k) {
                return true;
            }
        }
        return false;
    }
    if ni >= n.len() {
        return false;
    }
    if p[pi] == b'?' || p[pi] == n[ni] {
        return pattern_match_bytes(p, n, pi + 1, ni + 1);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde() {
        assert_eq!(expand_tilde("~/foo", "/home/u"), "/home/u/foo");
        assert_eq!(expand_tilde("~", "/home/u"), "/home/u");
        assert_eq!(expand_tilde("~bar", "/home/u"), "~bar");
    }

    #[test]
    fn variables() {
        unsafe { std::env::set_var("ISH_TEST_VAR", "hello") };
        assert_eq!(expand_variables("$ISH_TEST_VAR world"), "hello world");
        assert_eq!(expand_variables("${ISH_TEST_VAR}"), "${ISH_TEST_VAR}");
        // Undefined var → empty
        assert_eq!(expand_variables("$UNDEFINED_ISH_VAR"), "");
    }

    #[test]
    fn pattern_matching() {
        assert!(pattern_match("*.rs", "main.rs"));
        assert!(pattern_match("*.rs", "test.rs"));
        assert!(!pattern_match("*.rs", "main.py"));
        assert!(pattern_match("test?", "test1"));
        assert!(!pattern_match("test?", "test12"));
        assert!(pattern_match("*", "anything"));
    }
}
