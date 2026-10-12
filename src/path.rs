use fxhash::FxHashSet;
use std::path::PathBuf;

/// The one definition of "runnable command file": a regular file (not a
/// socket, device, or directory) with at least one execute bit.
fn is_executable_file(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFREG && mode & 0o111 != 0
}

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
///
/// The caller supplies the `$PATH` bytes from epsh's variable store — the
/// single source of truth for the environment. The OS process environment is
/// only a denv mirror and may be stale, so it is never consulted here.
pub struct PathCache {
    commands: FxHashSet<u64>,
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
            commands: FxHashSet::default(),
            path_hash: 0,
        }
    }

    /// Check if `cmd` is an executable on `$PATH` (given as raw bytes).
    /// Rebuilds the cache if `$PATH` has changed since the last check. A cache
    /// miss is confirmed with a direct lookup so a binary installed after the
    /// scan becomes valid without a restart or a full rescan per keystroke.
    pub fn contains(&mut self, cmd: &str, path_bytes: &[u8]) -> bool {
        self.ensure_fresh(path_bytes);
        let hash = fnv1a(cmd.as_bytes());
        if self.commands.contains(&hash) {
            return true;
        }
        if scan_path_bytes(cmd, path_bytes).is_some() {
            self.commands.insert(hash);
            return true;
        }
        false
    }

    /// Rebuild the cache if `$PATH` has changed.
    fn ensure_fresh(&mut self, path_bytes: &[u8]) {
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
        // rustix's safe directory iterator allocates, so these libc calls are
        // retained to preserve the zero-allocation PATH-cache path.
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
                if is_executable_file(st.st_mode) {
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

/// Collect executable names from `$PATH` (given as raw bytes) matching
/// `prefix` into `comp`. The PATH comes from epsh's variable store — the
/// source of truth for the environment.
pub fn complete_commands(prefix: &str, comp: &mut crate::complete::Completions, path_bytes: &[u8]) {
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
            if !is_executable_file(st.st_mode) {
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

/// Find the full path to `cmd` by scanning the given `$PATH` bytes (from
/// epsh's variable store — the source of truth for the environment).
pub fn scan_path(cmd: &str, path_bytes: &[u8]) -> Option<PathBuf> {
    scan_path_bytes(cmd, path_bytes)
}

fn scan_path_bytes(cmd: &str, path_bytes: &[u8]) -> Option<PathBuf> {
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
            && is_executable_file(st.st_mode)
        {
            use std::os::unix::ffi::OsStrExt;
            return Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..total])));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn binary_installed_after_scan_is_found_and_socket_is_not() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/tmp")
            .join(format!("path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().as_bytes().to_vec();

        let mut cache = PathCache::new();
        assert!(!cache.contains("newtool", &path));

        let tool = dir.join("newtool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(cache.contains("newtool", &path));

        let _sock = std::os::unix::net::UnixListener::bind(dir.join("sock")).unwrap();
        assert!(!cache.contains("sock", &path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
