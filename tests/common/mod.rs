use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub fn ish_binary() -> PathBuf {
    let path = std::env::var_os("ISH_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ish")));
    assert!(path.exists(), "ish binary not found at {}", path.display());
    path
}

pub fn scratch_dir(prefix: &str) -> PathBuf {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let mut template = parent.join(format!("{prefix}_XXXXXX")).as_os_str().as_bytes().to_vec();
    template.push(0);
    let created = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    assert!(!created.is_null(), "mkdtemp failed: {}", std::io::Error::last_os_error());
    template.pop();
    let path = PathBuf::from(std::ffi::OsString::from_vec(template));
    std::fs::canonicalize(&path).unwrap_or(path)
}
