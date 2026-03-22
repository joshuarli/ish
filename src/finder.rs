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
/// Results are sorted by depth (shallowest first) so shallow matches appear
/// before deep ones. This prevents a deep directory like `target/debug/deps/`
/// from flooding results before showing nearby files.
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

    // Collect more than `limit` to ensure diversity after depth-sorting.
    // Cap at 5x limit to bound scan time on huge trees.
    let scan_cap = limit.saturating_mul(5).max(1000);
    let mut entries: Vec<(usize, String)> = Vec::with_capacity(scan_cap.min(4096));

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

        let depth = entry.depth();
        let path = entry.path();
        let rel = path
            .strip_prefix(root_path)
            .unwrap_or(path)
            .to_string_lossy();

        entries.push((depth, rel.into_owned()));

        if entries.len() >= scan_cap {
            break;
        }
    }

    // Sort by depth (shallowest first), then alphabetically within same depth
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    entries
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
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
    fn find_shallow_results_first() {
        // Depth-sorted: files in src/ should appear before files in src/subdir/
        let results = find(".", "rs", false, 100);
        if results.len() >= 2 {
            // Count path components as a proxy for depth
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
