#![no_main]
use libfuzzer_sys::fuzz_target;
use ish::error::Error;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // Limit input size to prevent glob expansion from walking the entire filesystem
        if input.len() > 256 {
            return;
        }

        // Disable glob by using a path prefix that doesn't exist
        let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };

        // expand_word must never panic
        let _ = ish::expand::expand_word(input, "/nonexistent_home", &mut no_subst);
    }
});
