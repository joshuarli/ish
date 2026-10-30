use crate::report;
use std::os::unix::fs::MetadataExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Native directory listing: `l` hides dotfiles, `ll` shows them.
/// Each line is `name size age`; directories and symlinks omit the size.
/// Directories are blue, links cyan (bold red when broken), executables
/// bold green. Writes output to stdout directly. Returns 0 on success,
/// 1 on error.
pub fn list_dir(path: impl AsRef<std::path::Path>, show_hidden: bool) -> i32 {
    let path = path.as_ref();
    let now = now_secs();
    let entries = match read_entries(path, show_hidden, now) {
        Ok(e) => e,
        Err(e) => {
            let cmd = if show_hidden { "ll" } else { "l" };
            report!("ish: {cmd}: {}: {e}", path.display());
            return 1;
        }
    };

    if entries.is_empty() {
        return 0;
    }

    print!("{}", render(&entries));
    0
}

fn render(entries: &[Entry]) -> String {
    let prefix_len = |e: &Entry| {
        let mut len = e.display_name.chars().count();
        if let Some(ref target) = e.link_target {
            len += " -> ".chars().count() + target.chars().count();
        }
        len
    };
    let name_w = entries.iter().map(prefix_len).max().unwrap_or(0);
    let size_w = entries
        .iter()
        .filter_map(|e| e.size.as_ref())
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0);
    let age_w = entries
        .iter()
        .map(|e| e.age.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for e in entries {
        out.push_str(&e.color_start);
        out.push_str(&e.display_name);
        if !e.color_start.is_empty() {
            out.push_str("\x1b[0m");
        }
        if let Some(ref target) = e.link_target {
            out.push_str(&format!(" -> {target}"));
        }
        for _ in prefix_len(e)..name_w {
            out.push(' ');
        }
        out.push_str("  ");
        if size_w > 0 {
            if let Some(ref size) = e.size {
                for _ in size.chars().count()..size_w {
                    out.push(' ');
                }
                out.push_str(size);
            } else {
                for _ in 0..size_w {
                    out.push(' ');
                }
            }
            out.push_str("  ");
        }
        for _ in e.age.chars().count()..age_w {
            out.push(' ');
        }
        out.push_str(&e.age);
        out.push('\n');
    }
    out
}

struct Entry {
    size: Option<String>,
    age: String,
    display_name: String,
    link_target: Option<String>,
    color_start: String,
}

fn read_entries(
    path: impl AsRef<std::path::Path>,
    show_hidden: bool,
    now: i64,
) -> Result<Vec<Entry>, std::io::Error> {
    let path = path.as_ref();
    let top_meta = std::fs::symlink_metadata(path)?;
    let top_is_dir = if top_meta.file_type().is_symlink() {
        std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        top_meta.is_dir()
    };
    if !top_is_dir {
        let p = path;
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        return Ok(vec![build_entry(&name, p, &top_meta, now)]);
    }

    let mut entries = Vec::new();
    for dir_entry in std::fs::read_dir(path)? {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name().to_string_lossy().into_owned();

        if name == "." || name == ".." {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let full_path = dir_entry.path();
        let lmeta = match std::fs::symlink_metadata(&full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(build_entry(&name, &full_path, &lmeta, now));
    }

    entries.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });

    Ok(entries)
}

fn build_entry(
    name: &str,
    full_path: &std::path::Path,
    lmeta: &std::fs::Metadata,
    now: i64,
) -> Entry {
    let mode = lmeta.mode();
    let is_link = lmeta.file_type().is_symlink();
    let target_meta = if is_link {
        std::fs::metadata(full_path).ok()
    } else {
        None
    };
    let is_dir = if is_link {
        target_meta.as_ref().is_some_and(|m| m.is_dir())
    } else {
        lmeta.is_dir()
    };
    let is_exec = !is_dir && mode & 0o111 != 0;

    let link_target = if is_link {
        std::fs::read_link(full_path)
            .ok()
            .map(|p| sanitize_name(&p.to_string_lossy()))
    } else {
        None
    };

    let safe_name = sanitize_name(name);
    let display_name = if is_dir {
        format!("{safe_name}/")
    } else {
        safe_name
    };

    let color_start = if is_link && target_meta.is_none() {
        "\x1b[1;31m".to_string()
    } else if is_link {
        "\x1b[36m".to_string()
    } else if is_dir {
        "\x1b[34m".to_string()
    } else if mode & 0o4000 != 0 {
        "\x1b[31m".to_string()
    } else if is_exec {
        "\x1b[1;32m".to_string()
    } else {
        String::new()
    };

    Entry {
        size: if lmeta.is_file() {
            Some(human_size(lmeta.len()))
        } else {
            None
        },
        age: relative_age(lmeta.mtime(), now),
        display_name,
        link_target,
        color_start,
    }
}

fn sanitize_name(name: &str) -> String {
    if name.bytes().all(|b| b >= b' ' && b != 0x7f) {
        return name.to_string();
    }
    name.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

fn human_size(size: u64) -> String {
    if size < 1024 {
        format!("{size}B")
    } else if size < 1024 * 1024 {
        format!("{:.1}K", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1}M", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}G", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn relative_age(mtime: i64, now: i64) -> String {
    let diff = now.saturating_sub(mtime).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    const YEAR: i64 = 365 * DAY;
    if diff < MINUTE {
        format!("{diff}s")
    } else if diff < HOUR {
        format!("{}m", diff / MINUTE)
    } else if diff < DAY {
        format!("{}h", diff / HOUR)
    } else if diff < YEAR {
        format!("{}d", diff / DAY)
    } else {
        format!("{}y", diff / YEAR)
    }
}

#[cfg(test)]
mod tests {
    use super::{read_entries, relative_age};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tempdir(files: &[&str]) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ish_ls_test_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        dir
    }

    #[test]
    fn entry_names_replace_terminal_controls() {
        assert_eq!(super::sanitize_name("plain name"), "plain name");
        assert_eq!(
            super::sanitize_name("line\nbreak\t\u{0085}"),
            "line�break��"
        );
    }

    #[test]
    fn relative_age_boundaries() {
        assert_eq!(relative_age(100, 100), "0s");
        assert_eq!(relative_age(200, 100), "0s");
        assert_eq!(relative_age(96, 100), "4s");
        assert_eq!(relative_age(0, 59), "59s");
        assert_eq!(relative_age(0, 60), "1m");
        assert_eq!(relative_age(0, 5 * 60), "5m");
        assert_eq!(relative_age(0, 3599), "59m");
        assert_eq!(relative_age(0, 3600), "1h");
        assert_eq!(relative_age(0, 2 * 3600), "2h");
        assert_eq!(relative_age(0, 86399), "23h");
        assert_eq!(relative_age(0, 86400), "1d");
        assert_eq!(relative_age(0, 3 * 86400), "3d");
        assert_eq!(relative_age(0, 40 * 86400), "40d");
        assert_eq!(relative_age(0, 364 * 86400), "364d");
        assert_eq!(relative_age(0, 365 * 86400), "1y");
        assert_eq!(relative_age(0, 800 * 86400), "2y");
    }

    #[test]
    fn hidden_entries_filter_on_flag() {
        let dir = tempdir(&["visible", ".hidden"]);
        let now = super::now_secs();
        let visible = read_entries(&dir, false, now).unwrap();
        assert_eq!(visible.len(), 1);
        assert!(visible[0].display_name.contains("visible"));
        let all = read_entries(&dir, true, now).unwrap();
        assert_eq!(all.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirs_omit_size_files_keep_it() {
        let dir = tempdir(&["file.txt"]);
        std::fs::create_dir(dir.join("subdir")).unwrap();
        let entries = read_entries(&dir, false, super::now_secs()).unwrap();
        assert_eq!(entries.len(), 2);
        let file = entries
            .iter()
            .find(|e| e.display_name == "file.txt")
            .unwrap();
        assert!(file.size.is_some());
        let sub = entries
            .iter()
            .find(|e| e.display_name == "subdir/")
            .unwrap();
        assert!(sub.size.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rendered_columns_align() {
        let entry = |display_name: &str, size: Option<&str>, age: &str| super::Entry {
            size: size.map(str::to_owned),
            age: age.to_owned(),
            display_name: display_name.to_owned(),
            link_target: None,
            color_start: String::new(),
        };
        let entries = vec![
            entry("a.txt", Some("1B"), "4s"),
            entry("longname.txt", Some("10.6K"), "40d"),
            entry("d/", None, "2h"),
        ];
        let out = super::render(&entries);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "longname.txt  10.6K  40d");
        let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "columns should align: {widths:?}\n{out}"
        );
    }

    #[test]
    fn semantic_colors() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempdir(&["plain", "exec"]);
        std::fs::set_permissions(dir.join("exec"), std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink("plain", dir.join("good")).unwrap();
        symlink("missing", dir.join("broken")).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let now = super::now_secs();
        let color = |name: &str| {
            let p = dir.join(name);
            let m = std::fs::symlink_metadata(&p).unwrap();
            super::build_entry(name, &p, &m, now).color_start
        };
        assert_eq!(color("plain"), "");
        assert_eq!(color("exec"), "\x1b[1;32m");
        assert_eq!(color("good"), "\x1b[36m");
        assert_eq!(color("broken"), "\x1b[1;31m");
        assert_eq!(color("sub"), "\x1b[34m");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
