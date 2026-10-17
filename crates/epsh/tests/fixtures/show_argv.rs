use std::os::unix::ffi::OsStrExt;

fn main() {
    for arg in std::env::args_os().skip(1) {
        for byte in arg.as_os_str().as_bytes() {
            print!("{byte:02x}");
        }
        println!();
    }
}
