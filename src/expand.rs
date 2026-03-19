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
    last_status: i32,
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
        let w = expand_variables(&word, last_status);
        Cow::Owned(expand_command_subst(&w, exec_subst)?)
    } else {
        word
    };
    // Re-check for glob chars after expansion — $? contains '?' in the raw
    // token but expands to a number, so the original has_glob is stale.
    if has_glob && has_glob_chars(&word) {
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
    last_status: i32,
) -> Result<Vec<String>, Error> {
    let mut result = Vec::new();
    for word in argv {
        result.extend(expand_word(word, home, exec_subst, last_status)?);
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

/// Zero-alloc i32 to decimal string into a stack buffer.
fn itoa_i32(n: i32, buf: &mut [u8; 12]) -> &str {
    if n == 0 {
        buf[0] = b'0';
        // SAFETY: b'0' is valid UTF-8.
        return unsafe { std::str::from_utf8_unchecked(&buf[..1]) };
    }
    let negative = n < 0;
    // Use u32 to handle i32::MIN without overflow
    let mut v: u32 = if negative {
        (n as i64).unsigned_abs() as u32
    } else {
        n as u32
    };
    let mut pos = buf.len();
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if negative {
        pos -= 1;
        buf[pos] = b'-';
    }
    // SAFETY: digits and '-' are valid UTF-8.
    unsafe { std::str::from_utf8_unchecked(&buf[pos..]) }
}

/// Expand a `${...}` parameter substitution.
/// `start` points to the first byte after `${` (the variable name).
/// Returns the number of bytes consumed (from `start` to after the closing `}`).
fn expand_braced_var(word: &str, bytes: &[u8], start: usize, result: &mut String) -> usize {
    let mut i = start;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    let name = &word[start..i];

    if i < bytes.len() && bytes[i] == b'}' {
        // Simple ${VAR}
        i += 1;
        if !name.is_empty()
            && let Ok(val) = std::env::var(name)
        {
            result.push_str(&val);
        }
        return i - start;
    }

    if i >= bytes.len() || name.is_empty() {
        // Malformed — skip to }
        while i < bytes.len() && bytes[i] != b'}' {
            i += 1;
        }
        if i < bytes.len() {
            i += 1;
        }
        return i - start;
    }

    // Parameter substitution: find closing }
    let val = std::env::var(name).ok();
    let op_start = i;
    let mut depth = 1u32;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            0 => i += 2,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let operand = strip_literal(&word[op_start..i - 1]);
    let is_set = val.as_ref().is_some_and(|v| !v.is_empty());

    if let Some(default) = operand.strip_prefix(":-") {
        if is_set {
            result.push_str(val.as_deref().unwrap());
        } else {
            result.push_str(default);
        }
    } else if let Some(default) = operand.strip_prefix('-') {
        match &val {
            Some(v) => result.push_str(v),
            None => result.push_str(default),
        }
    } else if let Some(alt) = operand.strip_prefix(":+") {
        if is_set {
            result.push_str(alt);
        }
    } else if let Some(alt) = operand.strip_prefix('+') {
        if val.is_some() {
            result.push_str(alt);
        }
    } else if let Some(pat) = operand.strip_prefix('#') {
        if let Some(v) = &val {
            result.push_str(strip_prefix_glob(v, pat));
        }
    } else if let Some(pat) = operand.strip_prefix('%')
        && let Some(v) = &val
    {
        result.push_str(strip_suffix_glob(v, pat));
    }

    i - start
}

fn expand_variables(word: &str, last_status: i32) -> String {
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
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'?' {
                i += 2;
                let mut buf = [0u8; 12];
                let s = itoa_i32(last_status, &mut buf);
                result.push_str(s);
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                i += 2; // skip ${
                i += expand_braced_var(word, bytes, i, &mut result);
            } else {
                // $NAME — bare variable name
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

/// Remove shortest prefix matching a simple glob pattern (* only).
fn strip_prefix_glob<'a>(val: &'a str, pattern: &str) -> &'a str {
    if pattern == "*" {
        return "";
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        // *SUFFIX — find first occurrence of suffix
        if let Some(pos) = val.find(suffix) {
            return &val[pos + suffix.len()..];
        }
    } else if let Some(rest) = pattern.strip_suffix('*') {
        // PREFIX* — strip if starts with prefix
        if let Some(stripped) = val.strip_prefix(rest) {
            return stripped;
        }
    } else if let Some(stripped) = val.strip_prefix(pattern) {
        return stripped;
    }
    val
}

/// Remove shortest suffix matching a simple glob pattern (* only).
fn strip_suffix_glob<'a>(val: &'a str, pattern: &str) -> &'a str {
    if pattern == "*" {
        return "";
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        // PREFIX* — find last occurrence of prefix
        if let Some(pos) = val.rfind(prefix) {
            return &val[..pos];
        }
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        // *SUFFIX — strip if ends with suffix
        if let Some(stripped) = val.strip_suffix(suffix) {
            return stripped;
        }
    } else if let Some(stripped) = val.strip_suffix(pattern) {
        return stripped;
    }
    val
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

    let segs = &segments[seg_start..];
    let mut idx = 0;
    while idx < segs.len() {
        let seg = segs[idx];
        if seg.is_empty() {
            idx += 1;
            continue;
        }

        let is_last = idx == segs.len() - 1;

        if seg == "**" {
            // Merge ** with the next segment to avoid double traversal.
            // e.g. **/foo.rs → recurse into all dirs, match "foo.rs" during the walk.
            let next_pat = if idx + 1 < segs.len() {
                idx += 1;
                segs[idx]
            } else {
                "*" // bare ** at end matches everything
            };
            let match_is_last = idx == segs.len() - 1;
            let mut new_results = Vec::new();
            for dir in &results {
                collect_recursive_match(dir, next_pat, match_is_last, &mut new_results)?;
            }
            results = new_results;
        } else {
            // Match this segment against directory entries
            let mut new_results = Vec::new();
            for dir in &results {
                match_segment(dir, seg, is_last, &mut new_results);
            }
            results = new_results;
        }
        idx += 1;
    }

    results.sort();
    Ok(results)
}

/// Match entries in `dir` against `pattern`, collecting into `out`.
fn match_segment(dir: &str, pattern: &str, is_last: bool, out: &mut Vec<String>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.bytes().any(|b| b < b' ' || b == 0x7f) {
                continue;
            }
            if name.starts_with('.') && !pattern.starts_with('.') {
                continue;
            }
            if pattern_match(pattern, &name) {
                let path = join_path(dir, &name);
                if !is_last {
                    if std::fs::metadata(&path)
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                    {
                        out.push(path);
                    }
                } else {
                    out.push(path);
                }
            }
        }
    }
}

/// Recursively walk directories from `dir`, matching `pattern` at every level.
/// This merges the ** traversal with the next segment's matching in one pass.
fn collect_recursive_match(
    dir: &str,
    pattern: &str,
    is_last: bool,
    out: &mut Vec<String>,
) -> Result<(), Error> {
    // Match in current directory
    match_segment(dir, pattern, is_last, out);

    // Recurse into subdirectories
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || name.bytes().any(|b| b < b' ' || b == 0x7f) {
                    continue;
                }
                let child = join_path(dir, &name);
                collect_recursive_match(&child, pattern, is_last, out)?;
            }
        }
    }
    Ok(())
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "." {
        name.to_string()
    } else if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Simple glob pattern matching: * matches any chars, ? matches one char.
/// Iterative algorithm — O(n*m) worst case, no recursion, no stack overflow.
pub fn pattern_match(pattern: &str, name: &str) -> bool {
    let p = pattern.as_bytes();
    let n = name.as_bytes();
    let mut pi = 0;
    let mut ni = 0;
    let mut star_pi = usize::MAX; // pattern pos after last *
    let mut star_ni = 0; // name pos to retry from

    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = pi + 1;
            star_ni = ni;
            pi += 1;
        } else if star_pi != usize::MAX {
            // Backtrack: let the last * consume one more char
            star_ni += 1;
            ni = star_ni;
            pi = star_pi;
        } else {
            return false;
        }
    }
    // Consume trailing *s
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
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
        assert_eq!(expand_variables("$ISH_TEST_VAR world", 0), "hello world");
        assert_eq!(expand_variables("${ISH_TEST_VAR}", 0), "hello");
        assert_eq!(expand_variables("${ISH_TEST_VAR}!", 0), "hello!");
        // Undefined var → empty
        assert_eq!(expand_variables("$UNDEFINED_ISH_VAR", 0), "");
        assert_eq!(expand_variables("${UNDEFINED_ISH_VAR}", 0), "");
    }

    #[test]
    fn braced_var_default() {
        unsafe { std::env::set_var("ISH_SET", "val") };
        unsafe { std::env::remove_var("ISH_UNSET") };
        // ${VAR:-default} — default if unset or empty
        assert_eq!(expand_variables("${ISH_SET:-fallback}", 0), "val");
        assert_eq!(expand_variables("${ISH_UNSET:-fallback}", 0), "fallback");
        // ${VAR-default} — default if unset only
        assert_eq!(expand_variables("${ISH_SET-fallback}", 0), "val");
        assert_eq!(expand_variables("${ISH_UNSET-fallback}", 0), "fallback");
    }

    #[test]
    fn braced_var_alternate() {
        unsafe { std::env::set_var("ISH_SET", "val") };
        unsafe { std::env::remove_var("ISH_UNSET") };
        // ${VAR:+alt} — alt if set and non-empty
        assert_eq!(expand_variables("${ISH_SET:+yes}", 0), "yes");
        assert_eq!(expand_variables("${ISH_UNSET:+yes}", 0), "");
        // ${VAR+alt} — alt if set
        assert_eq!(expand_variables("${ISH_SET+yes}", 0), "yes");
        assert_eq!(expand_variables("${ISH_UNSET+yes}", 0), "");
    }

    #[test]
    fn braced_var_strip() {
        unsafe { std::env::set_var("ISH_PATH", "/home/user/file.txt") };
        // ${VAR#pattern} — remove shortest prefix
        assert_eq!(expand_variables("${ISH_PATH#*/}", 0), "home/user/file.txt");
        // ${VAR%pattern} — remove shortest suffix
        assert_eq!(expand_variables("${ISH_PATH%/*}", 0), "/home/user");
        // ${VAR%.ext}
        assert_eq!(expand_variables("${ISH_PATH%.txt}", 0), "/home/user/file");
    }

    #[test]
    fn last_status() {
        assert_eq!(expand_variables("$?", 0), "0");
        assert_eq!(expand_variables("$?", 127), "127");
        assert_eq!(expand_variables("exit:$?", 1), "exit:1");
    }

    #[test]
    fn strip_prefix_glob_cases() {
        assert_eq!(
            strip_prefix_glob("/home/user/file.txt", "*/"),
            "home/user/file.txt"
        );
        assert_eq!(
            strip_prefix_glob("/home/user/file.txt", "/home/"),
            "user/file.txt"
        );
        assert_eq!(strip_prefix_glob("foobar", "foo"), "bar");
        assert_eq!(strip_prefix_glob("foobar", "baz"), "foobar");
        assert_eq!(strip_prefix_glob("anything", "*"), "");
    }

    #[test]
    fn strip_suffix_glob_cases() {
        assert_eq!(strip_suffix_glob("/home/user/file.txt", "/*"), "/home/user");
        assert_eq!(
            strip_suffix_glob("/home/user/file.txt", ".txt"),
            "/home/user/file"
        );
        assert_eq!(strip_suffix_glob("foobar", "bar"), "foo");
        assert_eq!(strip_suffix_glob("foobar", "baz"), "foobar");
        assert_eq!(strip_suffix_glob("anything", "*"), "");
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
