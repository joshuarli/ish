use std::os::unix::fs::MetadataExt;

/// Native directory listing, equivalent to `ls -plAhG`.
/// Writes output to stdout directly. Returns 0 on success, 1 on error.
pub fn list_dir(path: &str) -> i32 {
    let entries = match read_entries(path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ish: l: {path}: {e}");
            return 1;
        }
    };

    if entries.is_empty() {
        return 0;
    }

    // Compute column widths for alignment
    let max_nlink = entries.iter().map(|e| e.nlink_str.len()).max().unwrap_or(0);
    let max_owner = entries.iter().map(|e| e.owner.len()).max().unwrap_or(0);
    let max_group = entries.iter().map(|e| e.group.len()).max().unwrap_or(0);
    let max_size = entries.iter().map(|e| e.size_str.len()).max().unwrap_or(0);

    let mut out = String::new();
    for e in &entries {
        out.push_str(&e.color_start);
        // mode nlink owner group size date name[/|@]
        out.push_str(&format!(
            "{} {:>nw$} {:<ow$}  {:<gw$}  {:>sw$} {} {}",
            e.mode_str,
            e.nlink_str,
            e.owner,
            e.group,
            e.size_str,
            e.date_str,
            e.display_name,
            nw = max_nlink,
            ow = max_owner,
            gw = max_group,
            sw = max_size,
        ));
        if !e.color_start.is_empty() {
            out.push_str("\x1b[0m");
        }
        if let Some(ref target) = e.link_target {
            out.push_str(&format!(" -> {target}"));
        }
        out.push('\n');
    }

    print!("{out}");
    0
}

struct Entry {
    mode_str: String,
    nlink_str: String,
    owner: String,
    group: String,
    size_str: String,
    date_str: String,
    display_name: String,
    link_target: Option<String>,
    color_start: String,
}

fn read_entries(path: &str) -> Result<Vec<Entry>, std::io::Error> {
    let mut entries = Vec::new();

    for dir_entry in std::fs::read_dir(path)? {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name().to_string_lossy().into_owned();

        // Skip . and .. (-A behavior)
        if name == "." || name == ".." {
            continue;
        }

        let full_path = dir_entry.path();
        let lmeta = std::fs::symlink_metadata(&full_path)?;
        let mode = lmeta.mode();
        let is_link = lmeta.file_type().is_symlink();
        let is_dir = if is_link {
            std::fs::metadata(&full_path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            lmeta.is_dir()
        };
        let is_exec = !is_dir && mode & 0o111 != 0;

        let link_target = if is_link {
            std::fs::read_link(&full_path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            None
        };

        let display_name = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };

        let color_start = if is_link {
            "\x1b[36m".to_string() // cyan
        } else if is_dir {
            "\x1b[34m".to_string() // blue
        } else if mode & libc::S_ISUID as u32 != 0 {
            "\x1b[31m".to_string() // red for setuid
        } else if is_exec {
            "\x1b[32m".to_string() // green
        } else {
            String::new()
        };

        entries.push(Entry {
            mode_str: format_mode(mode),
            nlink_str: lmeta.nlink().to_string(),
            owner: username(lmeta.uid()),
            group: groupname(lmeta.gid()),
            size_str: human_size(lmeta.size() as u64),
            date_str: format_time(lmeta.mtime()),
            display_name,
            link_target,
            color_start,
        });
    }

    // Sort case-insensitively
    entries.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });

    Ok(entries)
}

fn format_mode(mode: u32) -> String {
    let mut s = String::with_capacity(10);

    // File type
    s.push(match mode & libc::S_IFMT as u32 {
        m if m == libc::S_IFDIR as u32 => 'd',
        m if m == libc::S_IFLNK as u32 => 'l',
        m if m == libc::S_IFCHR as u32 => 'c',
        m if m == libc::S_IFBLK as u32 => 'b',
        m if m == libc::S_IFIFO as u32 => 'p',
        m if m == libc::S_IFSOCK as u32 => 's',
        _ => '-',
    });

    // User
    s.push(if mode & libc::S_IRUSR as u32 != 0 { 'r' } else { '-' });
    s.push(if mode & libc::S_IWUSR as u32 != 0 { 'w' } else { '-' });
    s.push(if mode & libc::S_ISUID as u32 != 0 {
        if mode & libc::S_IXUSR as u32 != 0 { 's' } else { 'S' }
    } else if mode & libc::S_IXUSR as u32 != 0 {
        'x'
    } else {
        '-'
    });

    // Group
    s.push(if mode & libc::S_IRGRP as u32 != 0 { 'r' } else { '-' });
    s.push(if mode & libc::S_IWGRP as u32 != 0 { 'w' } else { '-' });
    s.push(if mode & libc::S_ISGID as u32 != 0 {
        if mode & libc::S_IXGRP as u32 != 0 { 's' } else { 'S' }
    } else if mode & libc::S_IXGRP as u32 != 0 {
        'x'
    } else {
        '-'
    });

    // Other
    s.push(if mode & libc::S_IROTH as u32 != 0 { 'r' } else { '-' });
    s.push(if mode & libc::S_IWOTH as u32 != 0 { 'w' } else { '-' });
    s.push(if mode & libc::S_ISVTX as u32 != 0 {
        if mode & libc::S_IXOTH as u32 != 0 { 't' } else { 'T' }
    } else if mode & libc::S_IXOTH as u32 != 0 {
        'x'
    } else {
        '-'
    });

    s
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

fn format_time(mtime: i64) -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&mtime, &mut tm);
    }

    let month = match tm.tm_mon {
        0 => "Jan",
        1 => "Feb",
        2 => "Mar",
        3 => "Apr",
        4 => "May",
        5 => "Jun",
        6 => "Jul",
        7 => "Aug",
        8 => "Sep",
        9 => "Oct",
        10 => "Nov",
        11 => "Dec",
        _ => "???",
    };

    let six_months = 180 * 24 * 60 * 60;
    if (now - mtime).unsigned_abs() < six_months as u64 {
        format!("{} {:2} {:02}:{:02}", month, tm.tm_mday, tm.tm_hour, tm.tm_min)
    } else {
        format!("{} {:2}  {}", month, tm.tm_mday, tm.tm_year + 1900)
    }
}

fn username(uid: u32) -> String {
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return uid.to_string();
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

fn groupname(gid: u32) -> String {
    unsafe {
        let gr = libc::getgrgid(gid);
        if gr.is_null() {
            return gid.to_string();
        }
        std::ffi::CStr::from_ptr((*gr).gr_name)
            .to_string_lossy()
            .into_owned()
    }
}
