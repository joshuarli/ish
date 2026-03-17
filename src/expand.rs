use crate::error::Error;
use crate::parse::LITERAL;

/// Full expansion pipeline for a single word. Returns one or more strings
/// (glob can expand to multiple). The `exec_subst` callback runs command
/// substitutions and returns their stdout.
pub fn expand_word(
    word: &str,
    home: &str,
    exec_subst: &mut dyn FnMut(&str) -> Result<String, Error>,
) -> Result<Vec<String>, Error> {
    let word = expand_tilde(word, home);
    let word = expand_variables(&word);
    let word = expand_command_subst(&word, exec_subst)?;
    expand_glob(&word)
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
        return format!("{home}/{rest}");
    }
    word.to_string()
}

// -- Variable Expansion --

fn expand_variables(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == LITERAL {
            // Escaped char — pass through with marker
            result.push(LITERAL);
            i += 1;
            if i < chars.len() {
                result.push(chars[i]);
                i += 1;
            }
        } else if chars[i] == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '(' {
                // Command substitution — leave for later pass
                result.push('$');
                i += 1;
            } else {
                // Variable name
                i += 1;
                let mut name = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    name.push(chars[i]);
                    i += 1;
                }
                if name.is_empty() {
                    result.push('$');
                } else if let Ok(val) = std::env::var(&name) {
                    result.push_str(&val);
                }
                // Undefined var → empty string
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

// -- Command Substitution --

fn expand_command_subst(
    word: &str,
    exec_subst: &mut dyn FnMut(&str) -> Result<String, Error>,
) -> Result<String, Error> {
    let mut result = String::with_capacity(word.len());
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == LITERAL {
            result.push(LITERAL);
            i += 1;
            if i < chars.len() {
                result.push(chars[i]);
                i += 1;
            }
        } else if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
            // $(...) command substitution
            let start = i + 2;
            let end = find_matching_paren(&chars, start)?;
            let cmd: String = chars[start..end].iter().collect();
            // Strip LITERAL markers from the command before executing
            let cmd = cmd.replace(LITERAL, "");
            let output = exec_subst(&cmd)?;
            result.push_str(output.trim_end_matches('\n'));
            i = end + 1;
        } else if chars[i] == '`' {
            // Backtick command substitution
            let start = i + 1;
            let end = find_backtick_end(&chars, start)?;
            let cmd: String = chars[start..end].iter().collect();
            let cmd = cmd.replace(LITERAL, "");
            let output = exec_subst(&cmd)?;
            result.push_str(output.trim_end_matches('\n'));
            i = end + 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    Ok(result)
}

fn find_matching_paren(chars: &[char], start: usize) -> Result<usize, Error> {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() && depth > 0 {
        if chars[i] == LITERAL {
            i += 2; // skip escaped char
        } else if chars[i] == '(' {
            depth += 1;
            i += 1;
        } else if chars[i] == ')' {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    Err(Error::bad_substitution("unclosed $("))
}

fn find_backtick_end(chars: &[char], start: usize) -> Result<usize, Error> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == LITERAL {
            i += 2;
        } else if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '`' {
            i += 2;
        } else if chars[i] == '`' {
            return Ok(i);
        } else {
            i += 1;
        }
    }
    Err(Error::bad_substitution("unclosed backtick"))
}

// -- Glob Expansion --

fn expand_glob(word: &str) -> Result<Vec<String>, Error> {
    // Check if word has any unescaped glob chars
    if !has_glob_chars(word) {
        // No glob — just strip LITERAL markers and return
        return Ok(vec![strip_literal(word)]);
    }

    let pattern = strip_literal(word);
    let matches = glob_match(&pattern)?;

    if matches.is_empty() {
        return Err(Error::glob_no_match(&pattern));
    }

    Ok(matches)
}

fn has_glob_chars(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == LITERAL {
            i += 2; // skip escaped char
        } else if matches!(chars[i], '*' | '?') {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

fn strip_literal(s: &str) -> String {
    s.replace(LITERAL, "")
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
                match std::fs::read_dir(dir) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
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
                                // If not last segment, only keep directories
                                if !is_last {
                                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                        new_results.push(path);
                                    }
                                } else {
                                    new_results.push(path);
                                }
                            }
                        }
                    }
                    Err(_) => {}
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
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') {
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
        Err(_) => {}
    }
    Ok(())
}

/// Simple glob pattern matching: * matches any chars, ? matches one char.
fn pattern_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    pattern_match_inner(&p, &n, 0, 0)
}

fn pattern_match_inner(p: &[char], n: &[char], pi: usize, ni: usize) -> bool {
    if pi == p.len() {
        return ni == n.len();
    }
    if p[pi] == '*' {
        // * matches zero or more chars
        for k in ni..=n.len() {
            if pattern_match_inner(p, n, pi + 1, k) {
                return true;
            }
        }
        return false;
    }
    if ni >= n.len() {
        return false;
    }
    if p[pi] == '?' || p[pi] == n[ni] {
        return pattern_match_inner(p, n, pi + 1, ni + 1);
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
