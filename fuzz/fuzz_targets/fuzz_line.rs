#![no_main]
use libfuzzer_sys::fuzz_target;
use ish::line::LineBuffer;

/// Fuzz the line buffer by interpreting bytes as a sequence of editing operations.
/// Each byte maps to a specific operation. This exercises cursor management,
/// UTF-8 boundary handling, and kill ring interactions.
fuzz_target!(|data: &[u8]| {
    let mut lb = LineBuffer::new();

    for &byte in data {
        match byte % 16 {
            0 => { lb.insert_char('a'); }
            1 => { lb.insert_char('日'); } // multi-byte UTF-8
            2 => { lb.delete_back(); }
            3 => { lb.delete_forward(); }
            4 => { lb.move_left(); }
            5 => { lb.move_right(); }
            6 => { lb.move_home(); }
            7 => { lb.move_end(); }
            8 => { lb.move_word_left(); }
            9 => { lb.move_word_right(); }
            10 => { lb.kill_to_end(); }
            11 => { lb.kill_to_start(); }
            12 => { lb.kill_word_back(); }
            13 => { lb.yank(); }
            14 => { lb.insert_char(' '); }
            15 => { lb.insert_str("hello"); }
            _ => unreachable!(),
        }

        // Invariants that must always hold:
        assert!(lb.cursor() <= lb.text().len());
        assert!(lb.text().is_char_boundary(lb.cursor()));
    }
});
