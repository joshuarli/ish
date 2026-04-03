use std::collections::HashSet;
use std::path::PathBuf;

fn fnv1a(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// O(1) command existence cache built from $PATH directories.
/// Stores FNV-1a hashes of executable names in a `HashSet<u64>`.
pub struct PathCache {
    commands: HashSet<u64>,
    path_hash: u64,
}

impl Default for PathCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PathCache {
    pub fn new() -> Self {
        Self {
            commands: HashSet::new(),
            path_hash: 0,
        }
    }

    /// Check if `cmd` is an executable on $PATH.
    /// Rebuilds the cache if $PATH has changed since the last check.
    pub fn contains(&mut self, cmd: &str) -> bool {
        self.ensure_fresh();
        self.commands.contains(&fnv1a(cmd.as_bytes()))
    }

    /// Rebuild the cache if $PATH has changed.
    fn ensure_fresh(&mut self) {
        // SAFETY: Single-threaded shell. getenv returns a pointer into the
        // environment block, valid until the variable is next modified.
        let path_bytes = unsafe {
            let ptr = libc::getenv(c"PATH".as_ptr());
            if ptr.is_null() {
                if self.path_hash != 0 {
                    self.commands.clear();
                    self.path_hash = 0;
                }
                return;
            }
            std::ffi::CStr::from_ptr(ptr).to_bytes()
        };

        let current_hash = fnv1a(path_bytes);
        if current_hash == self.path_hash && !self.commands.is_empty() {
            return;
        }
        self.path_hash = current_hash;
        self.rebuild(path_bytes);
    }

    /// Scan each directory in $PATH using libc opendir/readdir, insert hashes
    /// of executable filenames.
    fn rebuild(&mut self, path_bytes: &[u8]) {
        self.commands.clear();

        let mut pathbuf = [0u8; 4096];

        for dir in path_bytes.split(|&b| b == b':') {
            if dir.is_empty() {
                continue;
            }
            // NUL-terminate the directory path for opendir
            if dir.len() >= pathbuf.len() {
                continue;
            }
            pathbuf[..dir.len()].copy_from_slice(dir);
            pathbuf[dir.len()] = 0;

            // SAFETY: pathbuf is NUL-terminated. opendir returns NULL on failure.
            let dp = unsafe { libc::opendir(pathbuf.as_ptr() as *const libc::c_char) };
            if dp.is_null() {
                continue;
            }

            loop {
                // SAFETY: readdir returns entries from an open directory handle.
                let entry = unsafe { libc::readdir(dp) };
                if entry.is_null() {
                    break;
                }

                // SAFETY: d_name is a NUL-terminated C string within the dirent.
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name_bytes = name.to_bytes();

                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }

                // Build "dir/name\0" for stat
                let total = dir.len() + 1 + name_bytes.len();
                if total >= pathbuf.len() {
                    continue;
                }
                // dir is already at pathbuf[..dir.len()]
                pathbuf[dir.len()] = b'/';
                pathbuf[dir.len() + 1..total].copy_from_slice(name_bytes);
                pathbuf[total] = 0;

                // SAFETY: pathbuf is NUL-terminated. stat fills a stack struct.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::stat(pathbuf.as_ptr() as *const libc::c_char, &mut st) } != 0 {
                    continue;
                }

                // Regular file with at least one execute bit
                if (st.st_mode & libc::S_IFREG != 0) && (st.st_mode & 0o111 != 0) {
                    self.commands.insert(fnv1a(name_bytes));
                }
            }

            // SAFETY: Close the directory handle opened above.
            unsafe {
                libc::closedir(dp);
            }
        }
    }
}

/// Collect executable names from $PATH matching `prefix` into `comp`.
pub fn complete_commands(prefix: &str, comp: &mut crate::complete::Completions) {
    // SAFETY: Single-threaded shell. getenv returns a pointer into the
    // environment block, valid until the variable is next modified.
    let path_bytes = unsafe {
        let ptr = libc::getenv(c"PATH".as_ptr());
        if ptr.is_null() {
            return;
        }
        std::ffi::CStr::from_ptr(ptr).to_bytes()
    };

    let prefix_bytes = prefix.as_bytes();
    let mut pathbuf = [0u8; 4096];

    for dir in path_bytes.split(|&b| b == b':') {
        if dir.is_empty() {
            continue;
        }
        if dir.len() >= pathbuf.len() {
            continue;
        }
        pathbuf[..dir.len()].copy_from_slice(dir);
        pathbuf[dir.len()] = 0;

        // SAFETY: pathbuf is NUL-terminated. opendir returns NULL on failure.
        let dp = unsafe { libc::opendir(pathbuf.as_ptr() as *const libc::c_char) };
        if dp.is_null() {
            continue;
        }

        loop {
            // SAFETY: readdir returns entries from an open directory handle.
            let entry = unsafe { libc::readdir(dp) };
            if entry.is_null() {
                break;
            }

            // SAFETY: d_name is a NUL-terminated C string within the dirent.
            let name_cstr = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let name_bytes = name_cstr.to_bytes();

            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            if !name_bytes.starts_with(prefix_bytes) {
                continue;
            }
            if name_bytes.first() == Some(&b'.') && !prefix_bytes.starts_with(b".") {
                continue;
            }

            // Build "dir/name\0" for stat
            let total = dir.len() + 1 + name_bytes.len();
            if total >= pathbuf.len() {
                continue;
            }
            pathbuf[dir.len()] = b'/';
            pathbuf[dir.len() + 1..total].copy_from_slice(name_bytes);
            pathbuf[total] = 0;

            // SAFETY: pathbuf is NUL-terminated. stat fills a stack struct.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::stat(pathbuf.as_ptr() as *const libc::c_char, &mut st) } != 0 {
                continue;
            }
            if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                continue;
            }
            if st.st_mode & 0o111 == 0 {
                continue;
            }

            let name = match std::str::from_utf8(name_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            comp.push(name, false, false, true);
        }

        // SAFETY: Close the directory handle opened above.
        unsafe {
            libc::closedir(dp);
        }
    }
}

/// Find the full path to `cmd` by scanning $PATH directories.
pub fn scan_path(cmd: &str) -> Option<PathBuf> {
    // SAFETY: Single-threaded shell. getenv returns a pointer into the
    // environment block, valid until the variable is next modified.
    let path_bytes = unsafe {
        let ptr = libc::getenv(c"PATH".as_ptr());
        if ptr.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(ptr).to_bytes()
    };

    // Stack buffer for "dir/cmd\0" — avoids all PathBuf/String allocations
    let mut buf = [0u8; 4096];
    let cmd_bytes = cmd.as_bytes();

    for dir in path_bytes.split(|&b| b == b':') {
        if dir.is_empty() {
            continue;
        }
        let total = dir.len() + 1 + cmd_bytes.len();
        if total >= buf.len() {
            continue;
        }
        buf[..dir.len()].copy_from_slice(dir);
        buf[dir.len()] = b'/';
        buf[dir.len() + 1..total].copy_from_slice(cmd_bytes);
        buf[total] = 0; // NUL terminator for stat

        // SAFETY: buf is NUL-terminated, stat writes into a stack struct.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(buf.as_ptr() as *const libc::c_char, &mut st) } == 0
            && (st.st_mode & libc::S_IFREG != 0)
            && (st.st_mode & 0o111 != 0)
        {
            use std::os::unix::ffi::OsStrExt;
            return Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..total])));
        }
    }
    None
}
