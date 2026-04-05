#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // epsh's lexer must never panic on any input
        let mut lex = epsh::lexer::Lexer::new(input);
        lex.recognize_reserved = false;
        loop {
            match lex.next_token() {
                Ok((epsh::lexer::Token::Eof, _)) | Err(_) => break,
                Ok((epsh::lexer::Token::Word(parts, _), _)) => {
                    let _ = epsh::lexer::parts_to_text(&parts);
                }
                Ok(_) => {}
            }
        }
    }
});
