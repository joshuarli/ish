use std::os::unix::ffi::OsStrExt;

fn main() {
    for key in std::env::args().skip(1) {
        match std::env::var_os(&key) {
            Some(value) => {
                for byte in value.as_os_str().as_bytes() {
                    print!("{byte:02x}");
                }
                println!();
            }
            None => println!(),
        }
    }
}
