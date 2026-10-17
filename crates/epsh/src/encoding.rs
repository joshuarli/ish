//! Reversible compatibility encoding for arbitrary shell bytes in UTF-8 strings.

const PUA_BASE: u32 = 0xE000;
const ESCAPE: char = '\u{E000}';

fn push_text(result: &mut String, text: &str) {
    for c in text.chars() {
        if c == ESCAPE || (PUA_BASE + 0x80..=PUA_BASE + 0xFF).contains(&(c as u32)) {
            result.push(ESCAPE);
        }
        result.push(c);
    }
}

/// Encode raw bytes as shell text. Invalid UTF-8 bytes use U+E080–U+E0FF;
/// literal occurrences of those characters and U+E000 receive a U+E000 prefix.
pub fn bytes_to_str(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len());
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                push_text(&mut result, text);
                break;
            }
            Err(error) => {
                let (valid, invalid) = remaining.split_at(error.valid_up_to());
                push_text(&mut result, std::str::from_utf8(valid).unwrap());
                result.push(char::from_u32(PUA_BASE + u32::from(invalid[0])).unwrap());
                remaining = &invalid[1..];
            }
        }
    }
    result
}

/// Decode compatibility-encoded shell text to its original bytes.
pub fn str_to_bytes(s: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        let cp = c as u32;
        if c == ESCAPE {
            let literal = chars.next().unwrap_or(ESCAPE);
            let mut buf = [0u8; 4];
            result.extend_from_slice(literal.encode_utf8(&mut buf).as_bytes());
        } else if (PUA_BASE + 0x80..=PUA_BASE + 0xFF).contains(&cp) {
            result.push((cp - PUA_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            result.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii() {
        let input = b"hello world";
        let s = bytes_to_str(input);
        assert_eq!(s, "hello world");
        assert_eq!(str_to_bytes(&s), input);
    }

    #[test]
    fn roundtrip_utf8() {
        let input = "héllo wörld".as_bytes();
        let s = bytes_to_str(input);
        assert_eq!(s, "héllo wörld");
        assert_eq!(str_to_bytes(&s), input);
    }

    #[test]
    fn roundtrip_invalid_byte() {
        let input = &[0x5B, 0xA3];
        let s = bytes_to_str(input);
        assert_eq!(s.len(), 4);
        let output = str_to_bytes(&s);
        assert_eq!(output, input);
    }

    #[test]
    fn roundtrip_mixed() {
        let input = &[b'a', 0x80, b'b', 0xFF, b'c'];
        let s = bytes_to_str(input);
        let output = str_to_bytes(&s);
        assert_eq!(output, input);
    }

    #[test]
    fn pure_utf8_passthrough() {
        let s = "\u{E000}hello";
        let encoded = bytes_to_str(s.as_bytes());
        assert_eq!(str_to_bytes(&encoded), s.as_bytes());
    }
}
