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
        // mode nlink owner group size date — no color
        out.push_str(&format!(
            "{} {:>nw$} {:<ow$}  {:<gw$}  {:>sw$} {} ",
            e.mode_str,
            e.nlink_str,
            e.owner,
            e.group,
            e.size_str,
            e.date_str,
            nw = max_nlink,
            ow = max_owner,
            gw = max_group,
            sw = max_size,
        ));
        // Color only wraps the name
        out.push_str(&e.color_start);
        out.push_str(&e.display_name);
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
    // If path is a file (or symlink to file), list just that entry.
    let top_meta = std::fs::symlink_metadata(path)?;
    let top_is_dir = if top_meta.file_type().is_symlink() {
        std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        top_meta.is_dir()
    };
    if !top_is_dir {
        let p = std::path::Path::new(path);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        return Ok(vec![build_entry(&name, p, &top_meta)]);
    }

    let mut entries = Vec::new();
    for dir_entry in std::fs::read_dir(path)? {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name().to_string_lossy().into_owned();

        // Skip . and .. (-A behavior)
        if name == "." || name == ".." {
            continue;
        }

        let full_path = dir_entry.path();
        let lmeta = match std::fs::symlink_metadata(&full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(build_entry(&name, &full_path, &lmeta));
    }

    // Sort case-insensitively
    entries.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });

    Ok(entries)
}

fn build_entry(name: &str, full_path: &std::path::Path, lmeta: &std::fs::Metadata) -> Entry {
    let mode = lmeta.mode();
    let is_link = lmeta.file_type().is_symlink();
    let is_dir = if is_link {
        std::fs::metadata(full_path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
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

    Entry {
        mode_str: format_mode(mode),
        nlink_str: lmeta.nlink().to_string(),
        owner: username(lmeta.uid()),
        group: groupname(lmeta.gid()),
        size_str: human_size(lmeta.size()),
        date_str: format_time(lmeta.mtime()),
        display_name,
        link_target,
        color_start,
    }
}

/// Replace control characters (newlines, tabs, etc.) in filenames with
/// Unicode replacement char to prevent terminal injection and output corruption.
fn sanitize_name(name: &str) -> String {
    if name.bytes().all(|b| b >= b' ' && b != 0x7f) {
        return name.to_string();
    }
    name.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

fn format_mode(mode: u32) -> String {
    let mut s = String::with_capacity(10);

    s.push(match mode & libc::S_IFMT as u32 {
        m if m == libc::S_IFDIR as u32 => 'd',
        m if m == libc::S_IFLNK as u32 => 'l',
        m if m == libc::S_IFCHR as u32 => 'c',
        m if m == libc::S_IFBLK as u32 => 'b',
        m if m == libc::S_IFIFO as u32 => 'p',
        m if m == libc::S_IFSOCK as u32 => 's',
        _ => '-',
    });

    // (read, write, exec, setbit, set_char, SET_CHAR)
    const PERMS: [(u32, u32, u32, u32, char, char); 3] = [
        (
            libc::S_IRUSR as u32,
            libc::S_IWUSR as u32,
            libc::S_IXUSR as u32,
            libc::S_ISUID as u32,
            's',
            'S',
        ),
        (
            libc::S_IRGRP as u32,
            libc::S_IWGRP as u32,
            libc::S_IXGRP as u32,
            libc::S_ISGID as u32,
            's',
            'S',
        ),
        (
            libc::S_IROTH as u32,
            libc::S_IWOTH as u32,
            libc::S_IXOTH as u32,
            libc::S_ISVTX as u32,
            't',
            'T',
        ),
    ];
    for &(r, w, x, set, sc, tc) in &PERMS {
        s.push(if mode & r != 0 { 'r' } else { '-' });
        s.push(if mode & w != 0 { 'w' } else { '-' });
        s.push(if mode & set != 0 {
            if mode & x != 0 { sc } else { tc }
        } else if mode & x != 0 {
            'x'
        } else {
            '-'
        });
    }

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
    // SAFETY: time(NULL) returns seconds since epoch, cannot fail meaningfully.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: zeroed tm is valid for localtime_r. localtime_r is thread-safe
    // (unlike localtime) and writes into our stack-allocated tm struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&mtime, &mut tm);
    }

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("???");

    let six_months = 180 * 24 * 60 * 60;
    if (now - mtime).unsigned_abs() < six_months as u64 {
        format!(
            "{} {:2} {:02}:{:02}",
            month, tm.tm_mday, tm.tm_hour, tm.tm_min
        )
    } else {
        format!("{} {:2}  {}", month, tm.tm_mday, tm.tm_year + 1900)
    }
}

fn username(uid: u32) -> String {
    // SAFETY: getpwuid returns a pointer to a static struct (or NULL).
    // We immediately copy pw_name into an owned String. Single-threaded
    // shell so no concurrent getpwuid calls can invalidate the pointer.
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
    // SAFETY: getgrgid returns a pointer to a static struct (or NULL).
    // We immediately copy gr_name into an owned String. Single-threaded.
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
