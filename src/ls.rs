use crate::report;
use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

/// Native directory listing, equivalent to `ls -plAhG`.
/// Writes output to stdout directly. Returns 0 on success, 1 on error.
pub fn list_dir(path: impl AsRef<std::path::Path>) -> i32 {
    let path = path.as_ref();
    let entries = match read_entries(path) {
        Ok(e) => e,
        Err(e) => {
            report!("ish: l: {}: {e}", path.display());
            return 1;
        }
    };

    if entries.is_empty() {
        return 0;
    }

    let max_nlink = entries.iter().map(|e| e.nlink_str.len()).max().unwrap_or(0);
    let max_owner = entries.iter().map(|e| e.owner.len()).max().unwrap_or(0);
    let max_group = entries.iter().map(|e| e.group.len()).max().unwrap_or(0);
    let max_size = entries.iter().map(|e| e.size_str.len()).max().unwrap_or(0);

    let mut out = String::new();
    for e in &entries {
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

fn read_entries(path: impl AsRef<std::path::Path>) -> Result<Vec<Entry>, std::io::Error> {
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
        return Ok(vec![build_entry(&name, p, &top_meta)?]);
    }

    let mut entries = Vec::new();
    for dir_entry in std::fs::read_dir(path)? {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name().to_string_lossy().into_owned();

        if name == "." || name == ".." {
            continue;
        }

        let full_path = dir_entry.path();
        let lmeta = match std::fs::symlink_metadata(&full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(build_entry(&name, &full_path, &lmeta)?);
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
) -> io::Result<Entry> {
    let mode = lmeta.mode();
    let file_type = lmeta.file_type();
    let is_link = file_type.is_symlink();
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
        "\x1b[36m".to_string()
    } else if is_dir {
        "\x1b[34m".to_string()
    } else if mode & 0o4000 != 0 {
        "\x1b[31m".to_string()
    } else if is_exec {
        "\x1b[32m".to_string()
    } else {
        String::new()
    };

    Ok(Entry {
        mode_str: format_mode(
            mode,
            if is_link {
                'l'
            } else if is_dir {
                'd'
            } else if file_type.is_char_device() {
                'c'
            } else if file_type.is_block_device() {
                'b'
            } else if file_type.is_fifo() {
                'p'
            } else if file_type.is_socket() {
                's'
            } else {
                '-'
            },
        ),
        nlink_str: lmeta.nlink().to_string(),
        owner: username(lmeta.uid())?,
        group: groupname(lmeta.gid())?,
        size_str: human_size(lmeta.size()),
        date_str: format_time(lmeta.mtime()),
        display_name,
        link_target,
        color_start,
    })
}

fn sanitize_name(name: &str) -> String {
    if name.bytes().all(|b| b >= b' ' && b != 0x7f) {
        return name.to_string();
    }
    name.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

fn format_mode(mode: u32, file_type: char) -> String {
    let mut s = String::with_capacity(10);

    s.push(file_type);

    const PERMS: [(u32, u32, u32, u32, char, char); 3] = [
        (0o400, 0o200, 0o100, 0o4000, 's', 'S'),
        (0o040, 0o020, 0o010, 0o2000, 's', 'S'),
        (0o004, 0o002, 0o001, 0o1000, 't', 'T'),
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
    // libc exposes the thread-safe localtime_r conversion that rustix lacks.
    // SAFETY: POSIX permits a null argument to request the current time.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: zero is valid for each integer and nullable pointer field in libc::tm.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `mtime` is readable and `tm` is writable for the duration of the call.
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

const INITIAL_NSS_BUFFER_SIZE: usize = 1024;

// rustix does not expose reentrant NSS account lookups.
struct NssBuffer {
    stack: [libc::c_char; INITIAL_NSS_BUFFER_SIZE],
    heap: Vec<libc::c_char>,
    len: usize,
}

impl NssBuffer {
    fn new() -> Self {
        Self {
            stack: [0; INITIAL_NSS_BUFFER_SIZE],
            heap: Vec::new(),
            len: INITIAL_NSS_BUFFER_SIZE,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut libc::c_char {
        if self.heap.is_empty() {
            self.stack.as_mut_ptr()
        } else {
            self.heap.as_mut_ptr()
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn grow(&mut self) -> io::Result<()> {
        let new_len = self.len.checked_mul(2).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "NSS record exceeds addressable memory",
            )
        })?;
        let additional = if self.heap.is_empty() {
            new_len
        } else {
            new_len - self.heap.len()
        };
        self.heap
            .try_reserve_exact(additional)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        self.heap.resize(new_len, 0);
        self.len = new_len;
        Ok(())
    }
}

fn nss_name(name: *const libc::c_char, database: &str) -> io::Result<String> {
    if name.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{database} record has no name"),
        ));
    }
    // SAFETY: reentrant NSS calls return NUL-terminated names in the live lookup buffer.
    Ok(unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned())
}

fn lookup_nss_name<Id, Record>(
    id: Id,
    database: &str,
    lookup: unsafe extern "C" fn(
        Id,
        *mut Record,
        *mut libc::c_char,
        usize,
        *mut *mut Record,
    ) -> libc::c_int,
    get_name: impl FnOnce(&Record) -> *mut libc::c_char,
) -> io::Result<String>
where
    Id: Copy + ToString,
{
    let mut buffer = NssBuffer::new();
    let mut record = MaybeUninit::<Record>::uninit();
    loop {
        let mut result = std::ptr::null_mut::<Record>();
        let buffer_len = buffer.len();
        let buffer_ptr = buffer.as_mut_ptr();
        // SAFETY: NSS writes through these valid output pointers and returns strings in `buffer`.
        let status = unsafe { lookup(id, record.as_mut_ptr(), buffer_ptr, buffer_len, &mut result) };
        if status == libc::ERANGE {
            buffer.grow()?;
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Ok(id.to_string());
        }
        // SAFETY: a successful non-null NSS result means libc initialized the record.
        let record = unsafe { record.assume_init() };
        return nss_name(get_name(&record), database);
    }
}

fn username(uid: u32) -> io::Result<String> {
    lookup_nss_name(uid, "passwd", libc::getpwuid_r, |record: &libc::passwd| {
        record.pw_name
    })
}

fn groupname(gid: u32) -> io::Result<String> {
    lookup_nss_name(gid, "group", libc::getgrgid_r, |record: &libc::group| {
        record.gr_name
    })
}

#[cfg(test)]
mod tests {
    use super::read_entries;

    #[test]
    fn entry_names_replace_terminal_controls() {
        assert_eq!(super::sanitize_name("plain name"), "plain name");
        assert_eq!(
            super::sanitize_name("line\nbreak\t\u{0085}"),
            "line�break��"
        );
    }

    #[test]
    fn concurrent_reads_keep_owner_and_group_names() {
        const WORKERS: usize = 8;
        const READS_PER_WORKER: usize = 256;

        let path = std::env::current_exe()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let expected = read_entries(&path).unwrap().remove(0);

        std::thread::scope(|scope| {
            let mut workers = Vec::with_capacity(WORKERS);
            for _ in 0..WORKERS {
                let path = &path;
                let owner = &expected.owner;
                let group = &expected.group;
                workers.push(scope.spawn(move || {
                    for _ in 0..READS_PER_WORKER {
                        let entries = read_entries(path).unwrap();
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].owner, *owner);
                        assert_eq!(entries[0].group, *group);
                    }
                }));
            }

            for worker in workers {
                worker.join().unwrap();
            }
        });
    }
}
