//! Bounded line diffs for `undo diff`.
//!
//! Common leading and trailing lines are trimmed; the remaining middle is
//! diffed exactly with an LCS table only when it is small, and summarized
//! otherwise. Binary content is never printed.

/// Bytes read from each side.
pub const READ_LIMIT: usize = 256 * 1024;
/// Largest middle section diffed line by line.
const MAX_MIDDLE: usize = 600;
const CONTEXT: usize = 2;

pub enum Diff {
    Same,
    Binary,
    Lines(Vec<String>),
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0) || std::str::from_utf8(data).is_err()
}

fn lines(data: &[u8]) -> Vec<&str> {
    // Only called after the UTF-8 check.
    let text = std::str::from_utf8(data).unwrap_or("");
    text.split_inclusive('\n').collect()
}

/// Diff two texts, producing at most `max_lines` output lines.
pub fn diff(old: &[u8], new: &[u8], max_lines: usize) -> Diff {
    if old == new {
        return Diff::Same;
    }
    if is_binary(old) || is_binary(new) {
        return Diff::Binary;
    }
    let a = lines(old);
    let b = lines(new);
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let am = &a[prefix..a.len() - suffix];
    let bm = &b[prefix..b.len() - suffix];
    let mut ops: Vec<(char, &str)> = Vec::new();
    for line in &a[prefix.saturating_sub(CONTEXT)..prefix] {
        ops.push((' ', line));
    }
    if am.len() <= MAX_MIDDLE && bm.len() <= MAX_MIDDLE {
        // LCS table over the changed middle.
        let (n, m) = (am.len(), bm.len());
        let mut t = vec![0u16; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if am[i] == bm[j] {
                    t[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push((' ', am[i]));
                i += 1;
                j += 1;
            } else if j < m && (i == n || t[i * (m + 1) + j + 1] > t[(i + 1) * (m + 1) + j]) {
                ops.push(('+', bm[j]));
                j += 1;
            } else {
                ops.push(('-', am[i]));
                i += 1;
            }
        }
    } else {
        let mut out = Vec::new();
        out.push(format!(
            "@@ {} lines replaced by {} lines (too large to diff line by line) @@",
            am.len(),
            bm.len()
        ));
        return Diff::Lines(out);
    }
    let tail_start = a.len() - suffix;
    for line in &a[tail_start..(tail_start + CONTEXT).min(a.len())] {
        ops.push((' ', line));
    }
    let mut out = vec![format!("@@ line {} @@", prefix.saturating_sub(CONTEXT) + 1)];
    for (tag, line) in ops {
        if out.len() >= max_lines {
            out.push("... (diff truncated)".into());
            break;
        }
        out.push(format!("{tag}{}", line.trim_end_matches('\n')));
    }
    Diff::Lines(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_small_changes_with_context() {
        let old = b"a\nb\nc\nd\ne\n";
        let new = b"a\nb\nX\nd\ne\nf\n";
        let Diff::Lines(lines) = diff(old, new, 50) else {
            panic!("expected a line diff");
        };
        assert_eq!(
            lines,
            vec!["@@ line 1 @@", " a", " b", "-c", "+X", " d", " e", "+f"]
        );
    }

    #[test]
    fn binary_and_identical_inputs_are_summarized() {
        assert!(matches!(diff(b"x\0y", b"x", 10), Diff::Binary));
        assert!(matches!(diff(b"same", b"same", 10), Diff::Same));
    }

    #[test]
    fn output_is_bounded() {
        let old: Vec<u8> = (0..500)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect();
        let Diff::Lines(lines) = diff(&old, b"", 20) else {
            panic!("expected a line diff");
        };
        assert_eq!(lines.len(), 21);
    }
}
