//! Byte-preserving encoding for non-UTF-8 shell data.
//!
//! POSIX shells handle arbitrary bytes. Rust strings are UTF-8. We bridge
//! the gap by mapping invalid UTF-8 bytes (0x80-0xFF) to Unicode Private
//! Use Area codepoints (U+E080-U+E0FF) on input, and mapping them back
//! to raw bytes on output. Internal processing uses valid UTF-8 throughout.
//!
//! This is the same idea as Python 3's `surrogateescape` error handler.

/// PUA base for encoding raw bytes. Byte 0xHH maps to U+E000+HH.
const PUA_BASE: u32 = 0xE000;

/// Decode raw bytes into a shell string, mapping invalid UTF-8 bytes
/// to PUA codepoints.
pub fn bytes_to_str(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Try to decode a valid UTF-8 sequence
        match std::str::from_utf8(&bytes[i..]) {
            Ok(s) => {
                result.push_str(s);
                break;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // Push the valid prefix
                if valid_up_to > 0 {
                    // SAFETY: from_utf8 confirmed these bytes are valid
                    result.push_str(unsafe {
                        std::str::from_utf8_unchecked(&bytes[i..i + valid_up_to])
                    });
                }
                i += valid_up_to;
                // Map the invalid byte to PUA
                let b = bytes[i] as u32;
                result.push(char::from_u32(PUA_BASE + b).unwrap());
                i += 1;
            }
        }
    }
    result
}

/// Encode a shell string back to raw bytes, mapping PUA codepoints
/// back to their original byte values.
pub fn str_to_bytes(s: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if (PUA_BASE + 0x80..=PUA_BASE + 0xFF).contains(&cp) {
            result.push((cp - PUA_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            result.extend_from_slice(encoded.as_bytes());
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
        let input = &[0x5B, 0xA3]; // [ followed by raw 0xA3
        let s = bytes_to_str(input);
        assert_eq!(s.len(), 4); // '[' (1) + PUA char (3 UTF-8 bytes)
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
        // Normal UTF-8 chars in the PUA range that are NOT our encoded bytes
        // should pass through unchanged (we only use PUA_BASE+0x80 to PUA_BASE+0xFF)
        let s = "\u{E000}hello"; // U+E000 is below our range (PUA_BASE+0x80)
        let bytes = str_to_bytes(s);
        let roundtrip = bytes_to_str(&bytes);
        assert_eq!(roundtrip, s);
    }
}
