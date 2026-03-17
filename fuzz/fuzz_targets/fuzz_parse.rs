#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // Parse must never panic on any input
        let _ = ish::parse::parse(input);
        let _ = ish::parse::needs_continuation(input);
    }
});
