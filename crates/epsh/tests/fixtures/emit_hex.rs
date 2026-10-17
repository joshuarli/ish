fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn main() {
    let hex = std::env::args().nth(1).expect("missing hex argument");
    let bytes = hex.as_bytes();
    assert!(bytes.len().is_multiple_of(2), "hex length must be even");
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = hex_nibble(pair[0]).expect("invalid hex");
        let lo = hex_nibble(pair[1]).expect("invalid hex");
        out.push((hi << 4) | lo);
    }
    use std::io::Write as _;
    std::io::stdout().write_all(&out).unwrap();
}
