//! Stack-allocated string for labels built in the paint path.

use std::fmt;

#[derive(Clone, Copy)]
pub struct FixedStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FixedStr<N> {
    pub const fn new() -> Self {
        FixedStr {
            buf: [0; N],
            len: 0,
        }
    }
    pub fn as_str(&self) -> &str {
        // Only whole UTF-8 sequences are ever appended (see write_str).
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for FixedStr<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for FixedStr<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = N - self.len;
        if s.len() <= room {
            self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
            self.len += s.len();
            Ok(())
        } else {
            // Copy as many whole chars as fit, then report overflow.
            let mut end = room;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            self.buf[self.len..self.len + end].copy_from_slice(&s.as_bytes()[..end]);
            self.len += end;
            Err(fmt::Error)
        }
    }
}

impl<const N: usize> PartialEq for FixedStr<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl<const N: usize> Eq for FixedStr<N> {}

impl<const N: usize> fmt::Debug for FixedStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
impl<const N: usize> fmt::Display for FixedStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    #[test]
    fn truncates_on_char_boundary() {
        let mut s = FixedStr::<4>::new();
        assert!(write!(s, "ab€").is_err()); // € is 3 bytes; only "ab" fits
        assert_eq!(s.as_str(), "ab");
    }
}
