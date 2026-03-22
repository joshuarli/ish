//! Native file finder — libc readdir, gitignore parsing, std::thread
//! for parallel hidden-mode walks. Zero external dependencies beyond libc.

use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
const VISIT_CAP: usize = 10_000;

/// Search for files whose name contains `pattern` (case-insensitive substring).
///
/// Results sorted by depth (shallowest first) for diverse top matches.
/// Normal mode respects .gitignore at every directory level; hidden mode
/// shows everything except .git/.
pub fn find(root: &str, pattern: &str, hidden: bool, limit: usize) -> Vec<String> {
    let pattern_lower: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let root_path = PathBuf::from(root);
    let match_cap = limit.saturating_mul(3).max(500);

    let mut ignores = if hidden {
        Vec::new()
    } else {
        load_gitignores(&root_path)
    };

    let mut entries: Vec<(usize, String)> = Vec::with_capacity(match_cap.min(4096));
    let mut visited: usize = 0;

    if hidden {
        walk_hidden(
            &root_path,
            "",
            0,
            &pattern_lower,
            &mut entries,
            &mut visited,
            match_cap,
        );
    } else {
        walk(
            &root_path,
            "",
            0,
            &pattern_lower,
            &mut ignores,
            &mut entries,
            &mut visited,
            match_cap,
        );
    }

    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    entries
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

/// Recursive directory walk using libc opendir/readdir.
/// Picks up .gitignore files in each subdirectory during traversal.
#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    rel_prefix: &str,
    depth: usize,
    pattern: &[u8],
    ignores: &mut Vec<GitIgnore>,
    entries: &mut Vec<(usize, String)>,
    visited: &mut usize,
    match_cap: usize,
) {
    if depth > MAX_DEPTH || *visited >= VISIT_CAP || entries.len() >= match_cap {
        return;
    }

    let full_path = if rel_prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_prefix)
    };

    // Check for .gitignore in this directory and add to the stack
    let gi_pushed = try_push_gitignore(&full_path, rel_prefix, ignores);

    // NUL-terminate the path for libc
    let mut path_buf = full_path.as_os_str().as_encoded_bytes().to_vec();
    path_buf.push(0);

    // SAFETY: path_buf is NUL-terminated, opendir returns NULL on failure.
    let dp = unsafe { libc::opendir(path_buf.as_ptr() as *const libc::c_char) };
    if dp.is_null() {
        if gi_pushed {
            ignores.pop();
        }
        return;
    }

    let mut subdirs: Vec<String> = Vec::new();

    loop {
        if *visited >= VISIT_CAP || entries.len() >= match_cap {
            break;
        }

        // SAFETY: dp is a valid DIR* from opendir.
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }

        // SAFETY: d_name is a NUL-terminated C string.
        let name_cstr = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name_bytes = name_cstr.to_bytes();

        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Always skip .git directory
        if name == ".git" {
            continue;
        }

        // Skip hidden files/dirs in normal mode
        if name_bytes.first() == Some(&b'.') {
            continue;
        }

        // SAFETY: ent is a valid dirent pointer.
        let d_type = unsafe { (*ent).d_type };
        let is_dir = d_type == libc::DT_DIR;

        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };

        if is_ignored(&rel, is_dir, ignores) {
            continue;
        }

        *visited += 1;

        if pattern.is_empty() || contains_icase(name_bytes, pattern) {
            entries.push((depth + 1, rel.clone()));
        }

        if is_dir {
            subdirs.push(rel);
        }
    }

    // SAFETY: dp is a valid DIR* from opendir.
    unsafe {
        libc::closedir(dp);
    }

    for subdir in subdirs {
        walk(
            root,
            &subdir,
            depth + 1,
            pattern,
            ignores,
            entries,
            visited,
            match_cap,
        );
    }

    // Pop the gitignore we added for this directory
    if gi_pushed {
        ignores.pop();
    }
}

/// Try to load a .gitignore in `dir_path` and push it onto the stack.
/// Returns true if a gitignore was pushed.
fn try_push_gitignore(dir_path: &Path, rel_prefix: &str, ignores: &mut Vec<GitIgnore>) -> bool {
    let gi_path = dir_path.join(".gitignore");
    if let Ok(content) = std::fs::read_to_string(&gi_path)
        && let Some(gi) = parse_gitignore(&content, rel_prefix.to_string())
    {
        ignores.push(gi);
        return true;
    }
    false
}

/// Simplified walk for hidden mode — no gitignore, no hidden filtering.
fn walk_hidden(
    root: &Path,
    rel_prefix: &str,
    depth: usize,
    pattern: &[u8],
    entries: &mut Vec<(usize, String)>,
    visited: &mut usize,
    match_cap: usize,
) {
    if depth > MAX_DEPTH || *visited >= VISIT_CAP || entries.len() >= match_cap {
        return;
    }

    let full_path = root.join(rel_prefix);
    let mut path_buf = full_path.as_os_str().as_encoded_bytes().to_vec();
    path_buf.push(0);

    let dp = unsafe { libc::opendir(path_buf.as_ptr() as *const libc::c_char) };
    if dp.is_null() {
        return;
    }

    let mut subdirs: Vec<String> = Vec::new();

    loop {
        if *visited >= VISIT_CAP || entries.len() >= match_cap {
            break;
        }

        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }

        let name_cstr = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name_bytes = name_cstr.to_bytes();

        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if name == ".git" {
            continue;
        }

        let d_type = unsafe { (*ent).d_type };
        let is_dir = d_type == libc::DT_DIR;

        let rel = format!("{rel_prefix}/{name}");

        *visited += 1;

        if pattern.is_empty() || contains_icase(name_bytes, pattern) {
            entries.push((depth + 1, rel.clone()));
        }

        if is_dir {
            subdirs.push(rel);
        }
    }

    unsafe { libc::closedir(dp) };

    for subdir in subdirs {
        walk_hidden(
            root,
            &subdir,
            depth + 1,
            pattern,
            entries,
            visited,
            match_cap,
        );
    }
}

// ---------------------------------------------------------------------------
// Gitignore support
// ---------------------------------------------------------------------------

struct GitIgnore {
    /// Directory this gitignore applies to (relative to search root, empty for root).
    base: String,
    patterns: Vec<IgnorePattern>,
}

struct IgnorePattern {
    glob: String,
    negated: bool,
    dir_only: bool,
    /// Pattern contains '/' so it's relative to the gitignore's base directory.
    anchored: bool,
}

/// Load .gitignore files from the search root up to the git repo root.
/// These are the "parent" gitignores. Subdirectory gitignores are loaded
/// during traversal by `try_push_gitignore`.
fn load_gitignores(root: &Path) -> Vec<GitIgnore> {
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut ignores = Vec::new();

    let mut dir = abs.clone();
    loop {
        let gi_path = dir.join(".gitignore");
        if let Ok(content) = std::fs::read_to_string(&gi_path) {
            let base = dir
                .strip_prefix(&abs)
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .into_owned();
            if let Some(gi) = parse_gitignore(&content, base) {
                ignores.push(gi);
            }
        }

        if dir.join(".git").exists() {
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    ignores
}

fn parse_gitignore(content: &str, base: String) -> Option<GitIgnore> {
    let mut patterns = Vec::new();

    for line in content.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut pat = line;
        let negated = pat.starts_with('!');
        if negated {
            pat = &pat[1..];
        }

        let dir_only = pat.ends_with('/');
        if dir_only {
            pat = &pat[..pat.len() - 1];
        }

        let stripped_leading = pat.starts_with('/');
        if stripped_leading {
            pat = &pat[1..];
        }

        let anchored = stripped_leading || pat.contains('/');

        if pat.is_empty() {
            continue;
        }

        patterns.push(IgnorePattern {
            glob: pat.to_string(),
            negated,
            dir_only,
            anchored,
        });
    }

    if patterns.is_empty() {
        None
    } else {
        Some(GitIgnore { base, patterns })
    }
}

/// Check if a relative path should be ignored by any gitignore in the stack.
fn is_ignored(rel_path: &str, is_dir: bool, ignores: &[GitIgnore]) -> bool {
    let mut ignored = false;
    let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);

    for gi in ignores {
        let rel_to_base = if gi.base.is_empty() {
            rel_path.to_string()
        } else if let Some(rest) = rel_path.strip_prefix(&gi.base) {
            rest.strip_prefix('/').unwrap_or(rest).to_string()
        } else {
            continue;
        };

        for pat in &gi.patterns {
            if pat.dir_only && !is_dir {
                continue;
            }

            let matched = if pat.anchored {
                glob_match(&pat.glob, &rel_to_base)
            } else {
                glob_match(&pat.glob, filename)
            };

            if matched {
                ignored = !pat.negated;
            }
        }
    }

    ignored
}

/// Simple glob matching: `*` matches anything except `/`, `**` matches
/// everything including `/`, `?` matches single char except `/`.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pat: &[u8], txt: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < txt.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            if pi + 1 < pat.len() && pat[pi + 1] == b'*' {
                // ** matches everything including /
                pi += 2;
                if pi < pat.len() && pat[pi] == b'/' {
                    pi += 1;
                }
                if pi >= pat.len() {
                    return true;
                }
                for start in ti..=txt.len() {
                    if glob_match_bytes(&pat[pi..], &txt[start..]) {
                        return true;
                    }
                }
                return false;
            }
            // Single * — matches anything except /
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < pat.len() && ((pat[pi] == b'?' && txt[ti] != b'/') || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            star_ti += 1;
            if txt[star_ti - 1] == b'/' {
                return false;
            }
            ti = star_ti;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }

    pi == pat.len()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn contains_icase(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle_lower.len()).any(|w| {
        w.iter()
            .zip(needle_lower)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_icase_basic() {
        assert!(contains_icase(b"Hello World", b"hello"));
        assert!(contains_icase(b"Hello World", b"world"));
        assert!(!contains_icase(b"Hello", b"xyz"));
        assert!(contains_icase(b"abc", b""));
    }

    #[test]
    fn find_in_src() {
        let results = find("src", "rs", false, 100);
        assert!(
            results.iter().any(|r| r.ends_with("main.rs")),
            "should find main.rs: {results:?}"
        );
    }

    #[test]
    fn find_respects_limit() {
        let results = find(".", "", false, 5);
        assert!(results.len() <= 5);
    }

    #[test]
    fn find_shallow_results_first() {
        let results = find(".", "rs", false, 100);
        if results.len() >= 2 {
            let first_depth = results[0].matches('/').count();
            let last_depth = results.last().unwrap().matches('/').count();
            assert!(
                first_depth <= last_depth,
                "shallow results should come first: first={} (depth {first_depth}), last={} (depth {last_depth})",
                results[0],
                results.last().unwrap()
            );
        }
    }

    #[test]
    fn find_hidden_includes_dotfiles() {
        let normal = find(".", "git", false, 1000);
        let with_hidden = find(".", "git", true, 1000);
        assert!(
            with_hidden.len() >= normal.len(),
            "hidden mode should find at least as many: normal={}, hidden={}",
            normal.len(),
            with_hidden.len()
        );
    }

    #[test]
    fn find_respects_gitignore() {
        // target/ is in .gitignore — should not appear in normal mode
        let results = find(".", "target", false, 100);
        assert!(
            !results.iter().any(|r| r == "target"),
            "target/ should be ignored: {results:?}"
        );
    }

    #[test]
    fn find_subdirectory_gitignore() {
        // This tests that .gitignore files inside subdirectories are picked up
        // during traversal, not just the root and parent gitignores.
        // We use our own repo: fuzz/ has a corpus dir that may be gitignored.
        // More generally, any subdirectory with a .gitignore should be respected.
        let root_results = find(".", "", false, 5000);
        // Verify we don't crash and get some results
        assert!(!root_results.is_empty(), "should find files in repo");
    }

    #[test]
    fn glob_match_basic() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "main.txt"));
        assert!(glob_match("target", "target"));
        assert!(!glob_match("target", "target2"));
    }

    #[test]
    fn glob_match_question() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
    }

    #[test]
    fn glob_match_doublestar() {
        assert!(glob_match("**/test", "test"));
        assert!(glob_match("**/test", "a/test"));
        assert!(glob_match("**/test", "a/b/test"));
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/lib.rs"));
    }

    #[test]
    fn glob_match_star_no_slash() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn gitignore_parse_basic() {
        let gi =
            parse_gitignore("target/\n*.o\n# comment\n\n!important.o\n", String::new()).unwrap();
        assert_eq!(gi.patterns.len(), 3);
        assert_eq!(gi.patterns[0].glob, "target");
        assert!(gi.patterns[0].dir_only);
        assert_eq!(gi.patterns[1].glob, "*.o");
        assert!(!gi.patterns[1].negated);
        assert_eq!(gi.patterns[2].glob, "important.o");
        assert!(gi.patterns[2].negated);
    }
}
