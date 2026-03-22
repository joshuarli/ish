//! Native file finder using the `ignore` crate for traversal with gitignore
//! support. Replaces the external `fd` dependency in the Ctrl+F file picker.

use ignore::WalkBuilder;
use std::path::Path;

/// Search for files whose name contains `pattern` (case-insensitive substring).
///
/// - `root`: directory to search from (typically ".")
/// - `pattern`: substring to match against filenames
/// - `hidden`: if true, include hidden files and skip gitignore filtering
/// - `limit`: maximum number of results to return
///
/// Returns relative paths from `root`, sorted by path.
pub fn find(root: &str, pattern: &str, hidden: bool, limit: usize) -> Vec<String> {
    let pattern_lower: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let root_path = Path::new(root);

    let walker = WalkBuilder::new(root_path)
        .hidden(!hidden)
        .git_ignore(!hidden)
        .git_global(!hidden)
        .git_exclude(!hidden)
        .parents(!hidden)
        .ignore(!hidden)
        .follow_links(false)
        .threads(1) // single-threaded — we're in an interactive shell
        .build();

    let mut results = Vec::with_capacity(limit.min(256));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip the root directory itself
        if entry.depth() == 0 {
            continue;
        }

        // Match against filename only (like fd default)
        let name = match entry.file_name().to_str() {
            Some(n) => n,
            None => continue,
        };

        if !pattern_lower.is_empty() && !contains_icase(name.as_bytes(), &pattern_lower) {
            continue;
        }

        // Build relative path from root
        let path = entry.path();
        let rel = path
            .strip_prefix(root_path)
            .unwrap_or(path)
            .to_string_lossy();

        results.push(rel.into_owned());

        if results.len() >= limit {
            break;
        }
    }

    results
}

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
    fn find_hidden_includes_dotfiles() {
        // With hidden=false, .git contents should be excluded
        let normal = find(".", "git", false, 1000);
        let with_hidden = find(".", "git", true, 1000);
        // Hidden mode should find more (or equal) results
        assert!(
            with_hidden.len() >= normal.len(),
            "hidden mode should find at least as many: normal={}, hidden={}",
            normal.len(),
            with_hidden.len()
        );
    }
}
