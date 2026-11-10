//! Secret wrapper: credentials must never reach logs or error messages, and
//! do not outlive their use in memory.

use std::fmt;
use zeroize::Zeroize;

/// A string that refuses to print itself, and is overwritten when dropped —
/// every clone of it too — so a token is not left in freed memory for a core
/// dump or a swap file to keep.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicit, greppable accessor: the only way to see the plaintext.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_prints_plaintext() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s}"), "***");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert!(!format!("{s:?} {s}").contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }
}
