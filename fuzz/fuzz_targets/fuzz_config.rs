#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // shell_words must never panic on any input
        let _ = ish::config::shell_words(input);

        // unquote must never panic
        let _ = ish::config::unquote(input);

        // expand_vars_simple must never panic
        let _ = ish::config::expand_vars_simple(input);
    }
});
