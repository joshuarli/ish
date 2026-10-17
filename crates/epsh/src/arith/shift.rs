pub(super) fn count(value: i64) -> Option<u32> {
    match value {
        0..=63 => Some(value as u32),
        _ => None,
    }
}
