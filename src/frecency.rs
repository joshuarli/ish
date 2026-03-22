//! Frecency-based directory jumping (`z` builtin).
//! Derives scores from shell history — no separate database needed.

use crate::history::History;

/// `z <query>` — jump to the highest-scoring directory matching query.
/// Scores are derived from cd/z commands in history, weighted by recency.
pub fn builtin_z(args: &[String], history: &History, home: &str) -> i32 {
    if args.is_empty() {
        // No args: cd home
        if let Err(e) = std::env::set_current_dir(home) {
            eprintln!("ish: z: {home}: {e}");
            return 1;
        }
        update_pwd();
        return 0;
    }

    let query = &args[0];

    // If argument is an existing directory, go there directly
    if std::path::Path::new(query).is_dir() {
        if let Err(e) = std::env::set_current_dir(query) {
            eprintln!("ish: z: {query}: {e}");
            return 1;
        }
        update_pwd();
        return 0;
    }

    // Scan history for cd/z commands, score by recency
    let query_lower: Vec<u8> = query.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let len = history.len();
    if len == 0 {
        eprintln!("ish: z: no match for '{query}'");
        return 1;
    }

    // Collect directory scores from history
    let mut scores: Vec<(String, f64)> = Vec::new();

    for i in 0..len {
        let entry = history.get(i);
        let dir = extract_cd_target(entry);
        let dir = match dir {
            Some(d) if !d.is_empty() && d != "-" && !d.starts_with('-') => d,
            _ => continue,
        };

        // Resolve ~ to home
        let resolved = if let Some(rest) = dir.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else if dir == "~" {
            home.to_string()
        } else {
            dir.to_string()
        };

        // Recency weight: later entries score higher (i/len gives 0..1)
        let weight = (i as f64 + 1.0) / len as f64;

        if let Some(entry) = scores.iter_mut().find(|(p, _)| *p == resolved) {
            entry.1 += weight;
        } else {
            scores.push((resolved, weight));
        }
    }

    // Filter to entries matching the query (case-insensitive substring of any path component)
    let mut matches: Vec<(&str, f64)> = scores
        .iter()
        .filter(|(path, _)| {
            let path_lower: Vec<u8> = path.bytes().map(|b| b.to_ascii_lowercase()).collect();
            path_lower
                .windows(query_lower.len())
                .any(|w| w == query_lower.as_slice())
        })
        .filter(|(path, _)| std::path::Path::new(path).is_dir())
        .map(|(path, score)| (path.as_str(), *score))
        .collect();

    // Sort by score descending
    matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let target = match matches.first() {
        Some((path, _)) => *path,
        None => {
            eprintln!("ish: z: no match for '{query}'");
            return 1;
        }
    };

    eprintln!("{target}");
    if let Err(e) = std::env::set_current_dir(target) {
        eprintln!("ish: z: {target}: {e}");
        return 1;
    }
    update_pwd();
    0
}

/// Extract the target directory from a cd/z history entry.
fn extract_cd_target(entry: &str) -> Option<&str> {
    let trimmed = entry.trim();
    // Handle: "cd dir", "z dir", "cd dir && ...", "cd dir; ..."
    let rest = trimmed
        .strip_prefix("cd ")
        .or_else(|| trimmed.strip_prefix("z "))?;

    // Take just the first argument (stop at whitespace, &&, ||, ;, |)
    let arg = rest
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '&' || c == '|' || c == ';')
        .next()?;

    if arg.is_empty() { None } else { Some(arg) }
}

fn update_pwd() {
    if let Ok(pwd) = std::env::current_dir() {
        crate::shell_setenv("PWD", &pwd.to_string_lossy());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_cd_simple() {
        assert_eq!(extract_cd_target("cd /tmp"), Some("/tmp"));
        assert_eq!(extract_cd_target("cd ~/d/ish"), Some("~/d/ish"));
        assert_eq!(extract_cd_target("z ish"), Some("ish"));
    }

    #[test]
    fn extract_cd_chained() {
        assert_eq!(extract_cd_target("cd /tmp && ls"), Some("/tmp"));
        assert_eq!(extract_cd_target("cd src; make"), Some("src"));
    }

    #[test]
    fn extract_cd_no_match() {
        assert_eq!(extract_cd_target("git commit -m fix"), None);
        assert_eq!(extract_cd_target("echo cd"), None);
    }

    #[test]
    fn extract_cd_dash() {
        assert_eq!(extract_cd_target("cd -"), Some("-"));
        // The caller filters out "-"
    }
}
