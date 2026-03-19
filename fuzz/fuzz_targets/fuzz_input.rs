#![no_main]
use libfuzzer_sys::fuzz_target;
use ish::error::Error;

// Fuzz the full input pipeline: parse → expand.
// Catches issues in the interaction between parsing and expansion,
// including alias-like patterns, deeply nested substitutions, and
// pathological glob patterns.
fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        if input.len() > 1024 {
            return;
        }

        // Parse must not panic
        let cmdline = match ish::parse::parse(input) {
            Ok(c) => c,
            Err(_) => return,
        };

        // Expand each command's argv — stub out command substitution
        let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };
        for (pipeline, _) in &cmdline.segments {
            for pcmd in &pipeline.commands {
                if pcmd.cmd.argv.is_empty() {
                    continue;
                }
                // expand_argv must not panic
                let _ = ish::expand::expand_argv(
                    &pcmd.cmd.argv,
                    "/nonexistent_home",
                    &mut no_subst,
                    0,
                );
            }
        }
    }
});
