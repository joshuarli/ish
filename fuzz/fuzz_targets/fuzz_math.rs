#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // eval must never panic on any input (depth guard is in eval itself)
        let _ = ish::math::eval(input);
    }
});
