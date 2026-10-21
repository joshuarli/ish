//! Frecency-based directory jumping (`z` builtin).
//! Derives scores from shell history — no separate database needed.

use crate::history::History;
use crate::static_word::static_word;
use epsh::ast::Command;
use epsh::parser::Parser;

/// `z <query>` — jump to the highest-scoring directory matching query.
/// Scores are derived from cd/z commands in history, weighted by recency.
pub fn builtin_z(args: &[String], history: &History, home: &str) -> i32 {
    if args.is_empty() {
        // No args: cd home
        if let Err(e) = std::env::set_current_dir(home) {
            eprintln!("ish: z: {home}: {e}");
            return 1;
        }
        return 0;
    }

    let query = &args[0];
    // An empty query would match every path and has no meaningful window.
    if query.is_empty() {
        eprintln!("ish: z: empty query");
        return 1;
    }

    // If argument is an existing directory, go there directly
    if std::path::Path::new(query).is_dir() {
        if let Err(e) = std::env::set_current_dir(query) {
            eprintln!("ish: z: {query}: {e}");
            return 1;
        }
        return 0;
    }

    // Scan history for cd/z commands, score by recency
    let query_lower: Vec<u8> = query.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let len = history.len();
    if len == 0 {
        eprintln!("ish: z: no match for '{query}'");
        return 1;
    }

    let scores = directory_scores(history, home);

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
    0
}

fn directory_scores(history: &History, home: &str) -> Vec<(String, f64)> {
    let len = history.len();
    let mut scores: Vec<(String, f64)> = Vec::new();
    let mut indices = Vec::with_capacity(len);
    history.command_indices_into(&mut indices);
    for (recency, i) in indices.into_iter().enumerate() {
        let entry = history.get(i);
        if entry.split_whitespace().next() != Some("cd") {
            continue;
        }
        let dir = match extract_cd_target(entry) {
            Some(d) if !d.is_empty() && d != "-" && !d.starts_with('-') => d,
            _ => continue,
        };

        // Candidate indices stay stable when another session repeats a
        // command. Chronological ordering supplies recency independently.
        let weight = (recency as f64 + 1.0) / len as f64;

        let mut credit = |resolved: String| {
            if let Some(entry) = scores.iter_mut().find(|(p, _)| *p == resolved) {
                entry.1 += weight;
            } else {
                scores.push((resolved, weight));
            }
        };

        if let Some(rest) = dir.strip_prefix("~/") {
            credit(format!("{home}/{rest}"));
        } else if dir == "~" {
            credit(home.to_string());
        } else if dir.starts_with('~') {
        } else if dir.starts_with('/') {
            credit(dir);
        } else {
            for cwd in history.directories(i).filter(|cwd| cwd.is_absolute()) {
                if let Some(cwd) = cwd.to_str() {
                    credit(join_lexically(cwd, &dir));
                }
            }
        }
    }
    scores
}

fn join_lexically(base: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn extract_cd_target(entry: &str) -> Option<String> {
    let program = Parser::new(entry).parse().ok()?;
    let command = first_command(program.commands.first()?)?;
    let Command::Simple { assigns, args, .. } = command else {
        return None;
    };
    if !assigns.is_empty() {
        return None;
    }

    let name = static_word(&args.first()?.parts)?;
    if !matches!(name.as_str(), "cd" | "z") {
        return None;
    }
    static_word(&args.get(1)?.parts)
}

fn first_command(command: &Command) -> Option<&Command> {
    match command {
        Command::And(left, _) | Command::Or(left, _) | Command::Sequence(left, _) => {
            first_command(left)
        }
        Command::Pipeline { commands, .. } => first_command(commands.first()?),
        command => Some(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_cd_simple() {
        assert_eq!(extract_cd_target("cd /tmp").as_deref(), Some("/tmp"));
        assert_eq!(extract_cd_target("cd ~/d/ish").as_deref(), Some("~/d/ish"));
    }

    #[test]
    fn extract_cd_from_command_list() {
        assert_eq!(extract_cd_target("cd /tmp && ls").as_deref(), Some("/tmp"));
        assert_eq!(extract_cd_target("cd src; make").as_deref(), Some("src"));
        assert_eq!(
            extract_cd_target("cd 'project space'").as_deref(),
            Some("project space")
        );
        assert_eq!(
            extract_cd_target("cd project\\ space").as_deref(),
            Some("project space")
        );
        assert_eq!(extract_cd_target("cd \"$PROJECT\"").as_deref(), None);
    }

    #[test]
    fn extract_cd_no_match() {
        assert_eq!(extract_cd_target("git commit -m fix"), None);
        assert_eq!(extract_cd_target("echo cd"), None);
    }

    #[test]
    fn empty_query_is_rejected() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/tmp")
            .join(format!("z-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let history = History::load_from(dir.join("history.log")).unwrap();
        assert_eq!(builtin_z(&[String::new()], &history, "/"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn score_of(scores: &[(String, f64)], path: &str) -> Option<f64> {
        scores.iter().find(|(p, _)| p == path).map(|(_, s)| *s)
    }

    #[test]
    fn relative_targets_resolve_against_their_stored_cwd() {
        let history = History::from_entries_in_dirs([
            ("cd src".to_string(), Some("/a".into())),
            ("cd src".to_string(), Some("/b".into())),
            ("cd ..".to_string(), Some("/a/b".into())),
            ("cd ./x/../y".to_string(), Some("/c".into())),
            ("cd lost".to_string(), None),
            ("cd /abs".to_string(), None),
        ]);
        let scores = directory_scores(&history, "/home");
        assert!(score_of(&scores, "/a/src").is_some());
        assert!(score_of(&scores, "/b/src").is_some());
        assert!(score_of(&scores, "/a").is_some());
        assert!(score_of(&scores, "/c/y").is_some());
        assert!(score_of(&scores, "/abs").is_some());
        assert!(scores.iter().all(|(p, _)| p.starts_with('/')));
        assert_eq!(scores.len(), 5);
    }

    #[test]
    fn extract_cd_dash() {
        assert_eq!(extract_cd_target("cd -").as_deref(), Some("-"));
    }
}
