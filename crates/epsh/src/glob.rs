use std::fs;
use std::path::{Path, PathBuf};

use std::os::unix::ffi::OsStrExt;

use crate::shell_bytes::ShellBytes;

/// Expand a glob pattern into matching filenames.
/// Returns sorted matches, or empty vec if no matches.
/// `cwd` is the working directory for resolving relative patterns.
pub fn glob(pattern: &str, cwd: &Path) -> Vec<String> {
    glob_shell(&crate::encoding::bytes_to_str(pattern.as_bytes()), cwd)
}

pub(crate) fn glob_shell(pattern: &str, cwd: &Path) -> Vec<String> {
    let pattern = ShellBytes::from_str_lossless(pattern);
    if pattern.is_empty() {
        return Vec::new();
    }

    let (absolute, components) = split_components(pattern.as_bytes());
    if absolute && components.is_empty() {
        return vec!["/".into()];
    }
    if components.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    if absolute {
        glob_recursive(
            &PathBuf::from("/"),
            &PathBuf::from("/"),
            &components,
            &mut results,
        );
    } else {
        glob_recursive(cwd, &PathBuf::new(), &components, &mut results);
    }
    sort_results(results)
}

fn split_components(pattern: &[u8]) -> (bool, Vec<ShellBytes>) {
    let absolute = pattern.starts_with(b"/");
    let components = pattern
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .map(|c| ShellBytes::from_vec(c.to_vec()))
        .collect();
    (absolute, components)
}

/// `fs_dir`: actual filesystem directory to read from
/// `display_dir`: path prefix for result strings (relative to original pattern)
fn glob_recursive(
    fs_dir: &Path,
    display_dir: &Path,
    components: &[ShellBytes],
    results: &mut Vec<String>,
) {
    if components.is_empty() {
        return;
    }

    let pattern = &components[0];
    let pattern_shell = pattern.to_shell_string();
    let remaining = &components[1..];

    if !has_glob_chars(&pattern_shell) {
        let literal = literal_component(&pattern_shell).to_os_string();
        let fs_candidate = fs_dir.join(&literal);
        let display_candidate = display_dir.join(&literal);
        if remaining.is_empty() {
            if fs_candidate.symlink_metadata().is_ok() {
                results.push(path_to_string(&display_candidate));
            }
        } else if fs_candidate.is_dir() {
            glob_recursive(&fs_candidate, &display_candidate, remaining, results);
        }
        return;
    }

    let entries = match fs::read_dir(fs_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_shell = ShellBytes::from_os_str(name.as_os_str()).to_shell_string();

        // Dotfiles are only matched if the pattern starts with '.'
        if name.as_os_str().as_bytes().starts_with(b".") && !pattern.as_bytes().starts_with(b".") {
            continue;
        }

        if fnmatch_shell(&pattern_shell, &name_shell) {
            let fs_full = fs_dir.join(&name);
            let display_full = display_dir.join(&name);
            if remaining.is_empty() {
                results.push(path_to_string(&display_full));
            } else if fs_full.is_dir() {
                glob_recursive(&fs_full, &display_full, remaining, results);
            }
        }
    }
}

fn literal_component(pattern: &str) -> ShellBytes {
    let mut literal = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if is_escape(ch as u32) {
            literal.push(chars.next().unwrap_or(ch));
        } else {
            literal.push(ch);
        }
    }
    ShellBytes::from_str_lossless(&literal)
}

/// Check if a string contains glob metacharacters.
/// CTLESC- and backslash-preceded chars are escaped (not glob-active).
pub fn has_glob_chars(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' | '[' => return true,
            c if c == crate::lexer::CTLESC || c == '\\' => {
                chars.next(); // skip escaped char
            }
            _ => {}
        }
    }
    false
}

/// POSIX fnmatch-style pattern matching.
/// Supports: * (any string), ? (any char), [...] (char class), \ (escape)
pub fn fnmatch(pattern: &str, string: &str) -> bool {
    fnmatch_shell(
        &crate::encoding::bytes_to_str(pattern.as_bytes()),
        &crate::encoding::bytes_to_str(string.as_bytes()),
    )
}

pub(crate) fn fnmatch_shell(pattern: &str, string: &str) -> bool {
    let pat: Vec<u32> = crate::encoding::shell_codepoints(pattern).collect();
    let s: Vec<u32> = crate::encoding::shell_codepoints(string).collect();
    fnmatch_inner(&pat, 0, &s, 0)
}

/// Check if a pattern char is an escape marker (CTLESC or backslash).
fn is_escape(c: u32) -> bool {
    c == '\\' as u32 || c == crate::lexer::CTLESC as u32
}

/// Read one char from pattern, handling escape markers (CTLESC and \).
/// Returns the literal char and advances pi.
fn read_bracket_char(pat: &[u32], pi: &mut usize) -> u32 {
    if is_escape(pat[*pi]) && *pi + 1 < pat.len() {
        *pi += 1;
        let c = pat[*pi];
        *pi += 1;
        c
    } else {
        let c = pat[*pi];
        *pi += 1;
        c
    }
}

fn fnmatch_inner(pat: &[u32], mut pi: usize, s: &[u32], mut si: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            c if c == '?' as u32 => {
                if si >= s.len() {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            c if c == '*' as u32 => {
                while pi < pat.len() && pat[pi] == '*' as u32 {
                    pi += 1;
                }
                if pi >= pat.len() {
                    return true;
                }
                for i in si..=s.len() {
                    if fnmatch_inner(pat, pi, s, i) {
                        return true;
                    }
                }
                return false;
            }
            c if c == '[' as u32 => {
                if si >= s.len() {
                    return false;
                }
                pi += 1;
                let negate = pi < pat.len() && pat[pi] == '!' as u32;
                if negate {
                    pi += 1;
                }

                let mut matched = false;
                let bracket_start = pi;
                while pi < pat.len() {
                    if pat[pi] == ']' as u32 && pi != bracket_start {
                        break;
                    }

                    let c1 = read_bracket_char(pat, &mut pi);
                    if pi + 1 < pat.len() && pat[pi] == '-' as u32 && pat[pi + 1] != ']' as u32 {
                        pi += 1;
                        let c2 = read_bracket_char(pat, &mut pi);
                        if s[si] >= c1 && s[si] <= c2 {
                            matched = true;
                        }
                    } else if s[si] == c1 {
                        matched = true;
                    }
                }
                if pi < pat.len() {
                    pi += 1;
                }

                if matched == negate {
                    return false;
                }
                si += 1;
            }
            c if is_escape(c) => {
                pi += 1;
                if pi >= pat.len() {
                    return false;
                }
                if si >= s.len() || s[si] != pat[pi] {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            c => {
                if si >= s.len() || s[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }

    si >= s.len()
}

fn path_to_string(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"./").unwrap_or(bytes);
    ShellBytes::from_vec(bytes.to_vec()).to_shell_string()
}

fn sort_results(mut results: Vec<String>) -> Vec<String> {
    results.sort_by_cached_key(|result| crate::encoding::str_to_bytes(result));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_literal() {
        assert!(fnmatch("hello", "hello"));
        assert!(!fnmatch("hello", "world"));
    }

    #[test]
    fn fnmatch_star() {
        assert!(fnmatch("*", "anything"));
        assert!(fnmatch("*.txt", "file.txt"));
        assert!(!fnmatch("*.txt", "file.rs"));
        assert!(fnmatch("h*o", "hello"));
        assert!(fnmatch("h*o", "ho"));
    }

    #[test]
    fn fnmatch_question() {
        assert!(fnmatch("?", "a"));
        assert!(!fnmatch("?", ""));
        assert!(!fnmatch("?", "ab"));
        assert!(fnmatch("h?llo", "hello"));
    }

    #[test]
    fn fnmatch_bracket() {
        assert!(fnmatch("[abc]", "a"));
        assert!(fnmatch("[abc]", "c"));
        assert!(!fnmatch("[abc]", "d"));
    }

    #[test]
    fn fnmatch_bracket_range() {
        assert!(fnmatch("[a-z]", "m"));
        assert!(!fnmatch("[a-z]", "M"));
        assert!(fnmatch("[0-9]", "5"));
    }

    #[test]
    fn fnmatch_bracket_negate() {
        assert!(!fnmatch("[!abc]", "a"));
        assert!(fnmatch("[!abc]", "d"));
    }

    #[test]
    fn fnmatch_escape() {
        assert!(fnmatch("\\*", "*"));
        assert!(!fnmatch("\\*", "a"));
    }

    #[test]
    fn fnmatch_complex() {
        assert!(fnmatch("*.tar.gz", "archive.tar.gz"));
        assert!(fnmatch("[Mm]ake*", "Makefile"));
        assert!(fnmatch("[Mm]ake*", "makefile.in"));
    }

    #[test]
    fn has_glob_chars_test() {
        assert!(has_glob_chars("*.txt"));
        assert!(has_glob_chars("file?"));
        assert!(has_glob_chars("[abc]"));
        assert!(!has_glob_chars("plain"));
        assert!(!has_glob_chars("escaped\\*"));
    }
}
