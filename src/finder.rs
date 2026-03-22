//! Native file finder — libc readdir, simple gitignore parsing, std::thread
//! for parallel hidden-mode walks. Zero external dependencies beyond libc.

use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
const VISIT_CAP: usize = 10_000;

/// Search for files whose name contains `pattern` (case-insensitive substring).
///
/// Results sorted by depth (shallowest first) for diverse top matches.
/// Normal mode respects .gitignore; hidden mode shows everything except .git/.
pub fn find(root: &str, pattern: &str, hidden: bool, limit: usize) -> Vec<String> {
    let pattern_lower: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let root_path = PathBuf::from(root);
    let match_cap = limit.saturating_mul(3).max(500);

    // Load gitignore rules from root to git toplevel
    let ignores = if hidden {
        Vec::new()
    } else {
        load_gitignores(&root_path)
    };

    let mut entries: Vec<(usize, String)> = Vec::with_capacity(match_cap.min(4096));
    let mut visited: usize = 0;

    walk(
        &root_path,
        "",
        0,
        &pattern_lower,
        hidden,
        &ignores,
        &mut entries,
        &mut visited,
        match_cap,
    );

    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    entries
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

/// Recursive directory walk using libc opendir/readdir.
/// Uses d_type for fast directory detection (no stat calls needed).
#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    rel_prefix: &str,
    depth: usize,
    pattern: &[u8],
    hidden: bool,
    ignores: &[GitIgnore],
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

    // NUL-terminate the path for libc
    let mut path_buf = full_path.as_os_str().as_encoded_bytes().to_vec();
    path_buf.push(0);

    // SAFETY: path_buf is NUL-terminated, opendir returns NULL on failure.
    let dp = unsafe { libc::opendir(path_buf.as_ptr() as *const libc::c_char) };
    if dp.is_null() {
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

        // Skip . and ..
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

        // Skip hidden files/dirs unless in hidden mode
        if !hidden && name_bytes.first() == Some(&b'.') {
            continue;
        }

        // d_type for fast directory detection (no stat needed)
        // SAFETY: ent is a valid dirent pointer.
        let d_type = unsafe { (*ent).d_type };
        let is_dir = d_type == libc::DT_DIR;

        // Build relative path
        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };

        // Check gitignore
        if !hidden && is_ignored(&rel, is_dir, ignores) {
            continue;
        }

        *visited += 1;

        // Check pattern match on filename
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

    // Recurse into subdirectories
    for subdir in subdirs {
        walk(
            root,
            &subdir,
            depth + 1,
            pattern,
            hidden,
            ignores,
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
    /// The glob pattern (without leading ! or trailing /).
    glob: String,
    /// Negation pattern (re-include).
    negated: bool,
    /// Only matches directories.
    dir_only: bool,
    /// Anchored: pattern contains '/' (not counting trailing), so it's relative to base.
    anchored: bool,
}

/// Load .gitignore files from the search root up to the git repo root.
fn load_gitignores(root: &Path) -> Vec<GitIgnore> {
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut ignores = Vec::new();

    // Walk up to find git root, collecting gitignore files
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

        // Stop at git root
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

        // Strip leading /  — anchors to the gitignore's directory
        let stripped_leading = pat.starts_with('/');
        if stripped_leading {
            pat = &pat[1..];
        }

        // A pattern with '/' (other than leading/trailing) is always anchored
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

/// Check if a relative path should be ignored.
fn is_ignored(rel_path: &str, is_dir: bool, ignores: &[GitIgnore]) -> bool {
    let mut ignored = false;

    for gi in ignores {
        // Path relative to the gitignore's base directory
        let rel_to_base = if gi.base.is_empty() {
            rel_path.to_string()
        } else if let Some(rest) = rel_path.strip_prefix(&gi.base) {
            rest.strip_prefix('/').unwrap_or(rest).to_string()
        } else {
            continue; // path is outside this gitignore's scope
        };

        let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);

        for pat in &gi.patterns {
            if pat.dir_only && !is_dir {
                continue;
            }

            let matched = if pat.anchored {
                // Match against path relative to gitignore base
                glob_match(&pat.glob, &rel_to_base)
            } else {
                // Match against filename only
                glob_match(&pat.glob, filename)
            };

            if matched {
                ignored = !pat.negated;
            }
        }
    }

    ignored
}

/// Simple glob matching: `*` matches anything except `/`, `**` matches everything
/// including `/`, `?` matches single char except `/`.
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
            // Check for **
            if pi + 1 < pat.len() && pat[pi + 1] == b'*' {
                // ** matches everything including /
                // Skip the ** and optional trailing /
                pi += 2;
                if pi < pat.len() && pat[pi] == b'/' {
                    pi += 1;
                }
                // Try matching the rest of the pattern at every position
                if pi >= pat.len() {
                    return true; // ** at end matches everything
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
            // Backtrack: * consumed one more char (but not /)
            star_ti += 1;
            if txt[star_ti - 1] == b'/' {
                return false; // * cannot cross /
            }
            ti = star_ti;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }

    // Consume trailing *'s
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }

    pi == pat.len()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Case-insensitive byte substring search.
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
        assert!(contains_icase(b"abc", b"")); // empty needle matches everything
    }

    #[test]
    fn find_in_src() {
        // Find .rs files in our own source tree
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
        assert!(!glob_match("*.rs", "src/main.rs")); // * doesn't cross /
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
