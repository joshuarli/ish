use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn main() {
    let path = std::env::args_os().nth(1).expect("missing path argument");
    print!("path=");
    for byte in path.as_os_str().as_bytes() {
        print!("{byte:02x}");
    }
    println!();

    let path = Path::new(&path);
    println!("exists={}", if path.exists() { 1 } else { 0 });
    let kind = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => "link",
        Ok(meta) if meta.is_dir() => "dir",
        Ok(meta) if meta.is_file() => "file",
        Ok(_) => "other",
        Err(_) => "other",
    };
    println!("kind={kind}");
}
