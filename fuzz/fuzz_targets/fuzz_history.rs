#![no_main]
use libfuzzer_sys::fuzz_target;
use ish::history;

/// Fuzz subsequence matching — the core fuzzy search algorithm.
/// This runs on every keystroke during Ctrl+R, so it must be robust.
fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }

    // Split data: first byte determines query length, rest is the text
    let query_len = (data[0] as usize).min(data.len() - 1).min(64);
    let query_bytes = &data[1..1 + query_len];
    let text_bytes = &data[1 + query_len..];

    if let (Ok(query_str), Ok(text_str)) = (
        std::str::from_utf8(query_bytes),
        std::str::from_utf8(text_bytes),
    ) {
        let query: Vec<char> = query_str.chars().flat_map(|c| c.to_lowercase()).collect();

        // subsequence_match must never panic
        if let Some(positions) = history::subsequence_match(&query, text_str) {
            // Verify invariants: positions are in-bounds and ascending
            let text_chars: Vec<char> = text_str.chars().collect();
            for (i, &pos) in positions.iter().enumerate() {
                assert!(pos < text_chars.len(), "position out of bounds");
                if i > 0 {
                    assert!(pos > positions[i - 1], "positions not ascending");
                }
            }
            assert_eq!(positions.len(), query.len(), "wrong number of positions");
        }
    }
});
