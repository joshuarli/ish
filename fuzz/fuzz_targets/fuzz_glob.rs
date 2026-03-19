#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }

    // Split: first byte picks pattern length, rest is name
    let pat_len = (data[0] as usize).min(data.len() - 1).min(128);
    let pat_bytes = &data[1..1 + pat_len];
    let name_bytes = &data[1 + pat_len..];

    if let (Ok(pattern), Ok(name)) = (std::str::from_utf8(pat_bytes), std::str::from_utf8(name_bytes)) {
        // pattern_match must never panic and must terminate in bounded time
        let _ = ish::expand::pattern_match(pattern, name);
    }
});
