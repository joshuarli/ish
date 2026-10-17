use std::borrow::Cow;
use std::ffi::{CString, NulError, OsStr, OsString};
use std::path::PathBuf;

use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// Byte-preserving shell data.
///
/// Shell values are byte sequences, not UTF-8 strings. This type stores the
/// raw bytes and provides explicit conversions at shell, OS, and libc
/// boundaries.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShellBytes(Vec<u8>);

impl ShellBytes {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn from_str_lossless(s: &str) -> Self {
        Self(crate::encoding::str_to_bytes(s))
    }

    pub fn to_shell_string(&self) -> String {
        crate::encoding::bytes_to_str(&self.0)
    }

    pub fn as_utf8_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    pub fn to_os_string(&self) -> OsString {
        OsString::from_vec(self.0.clone())
    }

    pub fn from_os_str(s: &OsStr) -> Self {
        Self(s.as_bytes().to_vec())
    }

    pub fn from_os_string(s: OsString) -> Self {
        Self(s.into_vec())
    }

    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.to_os_string())
    }

    pub fn to_cstring(&self) -> Result<CString, NulError> {
        CString::new(self.0.clone())
    }
}

impl std::fmt::Debug for ShellBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ShellBytes")
            .field(&self.to_shell_string())
            .finish()
    }
}

impl From<&str> for ShellBytes {
    fn from(value: &str) -> Self {
        Self::from_str_lossless(value)
    }
}

impl From<String> for ShellBytes {
    fn from(value: String) -> Self {
        Self::from_str_lossless(&value)
    }
}

impl From<Vec<u8>> for ShellBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::from_vec(value)
    }
}

impl AsRef<[u8]> for ShellBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl PartialEq<&str> for ShellBytes {
    fn eq(&self, other: &&str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<str> for ShellBytes {
    fn eq(&self, other: &str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<ShellBytes> for &str {
    fn eq(&self, other: &ShellBytes) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::ShellBytes;

    #[test]
    fn shell_roundtrip() {
        let bytes = ShellBytes::from_vec(vec![b'a', 0x80, b'b', 0xff]);
        assert_eq!(
            ShellBytes::from_str_lossless(&bytes.to_shell_string()),
            bytes
        );
    }

    #[test]
    fn utf8_view() {
        let bytes = ShellBytes::from("hello");
        assert_eq!(bytes.as_utf8_str(), Some("hello"));
    }
}
