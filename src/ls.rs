use crate::report;
use std::cmp::Ordering;
use std::fmt;
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
        .map(Field::len)
        .max()
        .unwrap_or(0);
    let age_w = entries.iter().map(|e| e.age.len()).max().unwrap_or(0);

    // One allocation for the whole listing: visible text plus slack for
    // separators and escape codes.
    let mut cap = entries.len();
    for e in entries {
        cap += prefix_len(e)
            + e.size.as_ref().map_or(0, Field::len)
            + e.age.len()
            + e.color_start.len()
            + e.age_shade.len()
            + 32;
    }
    let mut out = String::with_capacity(cap);
    for e in entries {
        out.push_str(e.color_start);
        out.push_str(&e.display_name);
        if !e.color_start.is_empty() {
            out.push_str("\x1b[0m");
        }
        if let Some(ref target) = e.link_target {
            out.push_str(" -> ");
            out.push_str(target);
        }
        for _ in prefix_len(e)..name_w {
            out.push(' ');
        }
        out.push_str("  ");
        if size_w > 0 {
            if let Some(ref size) = e.size {
                for _ in size.len()..size_w {
                    out.push(' ');
                }
                out.push_str(size.as_str());
            } else {
                for _ in 0..size_w {
                    out.push(' ');
                }
            }
            out.push_str("  ");
        }
        for _ in e.age.len()..age_w {
            out.push(' ');
        }
        if !e.age_shade.is_empty() {
            out.push_str(e.age_shade);
        }
        out.push_str(e.age.as_str());
        if !e.age_shade.is_empty() {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

struct Entry {
    size: Option<Field>,
    age: Field,
    age_shade: &'static str,
    display_name: String,
    link_target: Option<String>,
    color_start: &'static str,
}

/// Stack-allocated ASCII field for sizes and ages: formatted with no heap
/// use. All writers emit ASCII, so byte length is the display width.
#[derive(Debug)]
struct Field {
    buf: [u8; 24],
    len: u8,
}

impl Field {
    fn len(&self) -> usize {
        self.len as usize
    }

    fn as_str(&self) -> &str {
        str::from_utf8(&self.buf[..self.len()]).expect("ls fields are ASCII")
    }
}

impl PartialEq<&str> for Field {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<'a> From<&'a str> for Field {
    fn from(text: &'a str) -> Self {
        field(format_args!("{text}"))
    }
}

struct FieldWriter {
    buf: [u8; 24],
    len: usize,
}

impl fmt::Write for FieldWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let bytes = text.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

fn field(args: fmt::Arguments) -> Field {
    let mut out = FieldWriter { buf: [0; 24], len: 0 };
    fmt::write(&mut out, args).expect("ls field fits in 24 bytes");
    Field { buf: out.buf, len: out.len as u8 }
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
        return Ok(vec![build_entry(&name, Some(p), &top_meta, now)]);
    }

    let mut entries = Vec::new();
    for dir_entry in std::fs::read_dir(path)? {
        let dir_entry = dir_entry?;
        let os_name = dir_entry.file_name();
        let name = os_name.to_string_lossy();

        if name == "." || name == ".." {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        // fstatat on the open dir: same lstat data as symlink_metadata with
        // no path join and no allocation. Only symlinks pay for the joined
        // path below, for the target stat and readlink.
        let lmeta = match dir_entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let store;
        let for_link = if lmeta.file_type().is_symlink() {
            store = dir_entry.path();
            Some(store.as_path())
        } else {
            None
        };
        entries.push(build_entry(&name, for_link, &lmeta, now));
    }

    entries.sort_by(|a, b| caseless_cmp(&a.display_name, &b.display_name));

    Ok(entries)
}

/// Case-insensitive ordering without allocating: `str::to_lowercase` is
/// exactly `chars().flat_map(char::to_lowercase).collect()`, and comparing
/// those scalar streams matches comparing the lowered strings, since UTF-8
/// byte order preserves scalar order.
fn caseless_cmp(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(|c| c.to_lowercase())
        .cmp(b.chars().flat_map(|c| c.to_lowercase()))
}

fn build_entry(
    name: &str,
    full_path: Option<&std::path::Path>,
    lmeta: &std::fs::Metadata,
    now: i64,
) -> Entry {
    let mode = lmeta.mode();
    let is_link = lmeta.file_type().is_symlink();
    let target_meta = match full_path {
        Some(p) if is_link => std::fs::metadata(p).ok(),
        _ => None,
    };
    let is_dir = if is_link {
        target_meta.as_ref().is_some_and(|m| m.is_dir())
    } else {
        lmeta.is_dir()
    };
    let is_exec = !is_dir && mode & 0o111 != 0;

    let link_target = match full_path {
        Some(p) if is_link => std::fs::read_link(p)
            .ok()
            .map(|p| sanitize_name(&p.to_string_lossy())),
        _ => None,
    };

    let safe_name = sanitize_name(name);
    let display_name = if is_dir {
        format!("{safe_name}/")
    } else {
        safe_name
    };

    let color_start: &'static str = if is_link && target_meta.is_none() {
        "\x1b[1;31m"
    } else if is_link {
        "\x1b[36m"
    } else if is_dir {
        "\x1b[34m"
    } else if mode & 0o4000 != 0 {
        "\x1b[31m"
    } else if is_exec {
        "\x1b[1;32m"
    } else {
        ""
    };

    let diff = age_diff(lmeta.mtime(), now);

    Entry {
        size: if lmeta.is_file() {
            Some(human_size(lmeta.len()))
        } else {
            None
        },
        age: relative_age_str(diff),
        age_shade: age_shade(diff),
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

fn human_size(size: u64) -> Field {
    if size < 1024 {
        field(format_args!("{size}B"))
    } else if size < 1024 * 1024 {
        field(format_args!("{:.1}K", size as f64 / 1024.0))
    } else if size < 1024 * 1024 * 1024 {
        field(format_args!("{:.1}M", size as f64 / (1024.0 * 1024.0)))
    } else {
        field(format_args!("{:.1}G", size as f64 / (1024.0 * 1024.0 * 1024.0)))
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn age_diff(mtime: i64, now: i64) -> i64 {
    now.saturating_sub(mtime).max(0)
}

#[cfg(test)]
fn relative_age(mtime: i64, now: i64) -> Field {
    relative_age_str(age_diff(mtime, now))
}

fn relative_age_str(diff: i64) -> Field {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    const YEAR: i64 = 365 * DAY;
    if diff < MINUTE {
        field(format_args!("{diff}s"))
    } else if diff < HOUR {
        field(format_args!("{}m", diff / MINUTE))
    } else if diff < DAY {
        field(format_args!("{}h", diff / HOUR))
    } else if diff < YEAR {
        field(format_args!("{}d", diff / DAY))
    } else {
        field(format_args!("{}y", diff / YEAR))
    }
}

/// Grey ramp for the age column, keyed on absolute age. Files from the last
/// day keep the default color, then fade in small steps across the first
/// week; week-old files are already clearly grey and older files keep
/// fading toward dark grey, floored at a shade that stays readable on dark
/// terminals.
fn age_shade(diff: i64) -> &'static str {
    const DAY: i64 = 86400;
    if diff < DAY {
        ""
    } else if diff < 2 * DAY {
        "\x1b[38;5;252m"
    } else if diff < 4 * DAY {
        "\x1b[38;5;250m"
    } else if diff < 7 * DAY {
        "\x1b[38;5;248m"
    } else if diff < 30 * DAY {
        "\x1b[38;5;246m"
    } else if diff < 90 * DAY {
        "\x1b[38;5;242m"
    } else {
        "\x1b[38;5;240m"
    }
}

#[cfg(test)]
mod tests {
    use super::{age_shade, read_entries, relative_age};
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
    fn age_shade_steps() {
        const DAY: i64 = 86400;
        assert_eq!(age_shade(0), "");
        assert_eq!(age_shade(23 * 3600), "");
        assert_eq!(age_shade(DAY), "\x1b[38;5;252m");
        assert_eq!(age_shade(2 * DAY - 1), "\x1b[38;5;252m");
        assert_eq!(age_shade(2 * DAY), "\x1b[38;5;250m");
        assert_eq!(age_shade(4 * DAY - 1), "\x1b[38;5;250m");
        assert_eq!(age_shade(4 * DAY), "\x1b[38;5;248m");
        assert_eq!(age_shade(6 * DAY), "\x1b[38;5;248m");
        assert_eq!(age_shade(7 * DAY), "\x1b[38;5;246m");
        assert_eq!(age_shade(29 * DAY), "\x1b[38;5;246m");
        assert_eq!(age_shade(30 * DAY), "\x1b[38;5;242m");
        assert_eq!(age_shade(89 * DAY), "\x1b[38;5;242m");
        assert_eq!(age_shade(90 * DAY), "\x1b[38;5;240m");
        assert_eq!(age_shade(800 * DAY), "\x1b[38;5;240m");
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn rendered_columns_align() {
        use super::Field;
        let entry = |display_name: &str, size: Option<&str>, age: &str| super::Entry {
            size: size.map(Field::from),
            age: Field::from(age),
            age_shade: "",
            display_name: display_name.to_owned(),
            link_target: None,
            color_start: "",
        };
        let entries = vec![
            entry("a.txt", Some("10B"), "2h"),
            entry("longname.txt", Some("10.6K"), "40d"),
            entry("d/", None, "3d"),
        ];
        let out = super::render(&entries);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        let visible: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(visible[1], "longname.txt  10.6K  40d");
        assert_eq!(visible[0], "a.txt           10B   2h");
        assert_eq!(visible[2], format!("d/{}3d", " ".repeat(20)));
        // 20 = 10 name pad + 2 + 5 size + 2 + 1 age pad.
        let widths: Vec<usize> = visible.iter().map(|l| l.chars().count()).collect();
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
            super::build_entry(name, Some(&p), &m, now).color_start
        };
        assert_eq!(color("plain"), "");
        assert_eq!(color("exec"), "\x1b[1;32m");
        assert_eq!(color("good"), "\x1b[36m");
        assert_eq!(color("broken"), "\x1b[1;31m");
        assert_eq!(color("sub"), "\x1b[34m");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
