//! Incremental line splitting for files that are appended to while we read.
//!
//! Only complete lines (ending in `\n`) are emitted; a trailing partial line
//! is buffered and prepended to the next chunk, so a torn final line never
//! produces a parse error or a dropped record.

#[derive(Default)]
pub struct LineBuffer {
    partial: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; returns the complete lines it terminated.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            lines.push(String::from_utf8_lossy(&self.partial[start..end]).into_owned());
            start = end + 1;
        }
        self.partial.drain(..start);
        lines
    }

    /// The buffered partial line, if any. Used at end-of-file during a full
    /// scan, where a file that simply doesn't end in `\n` may still hold one
    /// complete final line.
    pub fn take_partial(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            None
        } else {
            let s = String::from_utf8_lossy(&self.partial).into_owned();
            self.partial.clear();
            Some(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_complete_lines() {
        let mut buf = LineBuffer::new();
        assert_eq!(buf.push(b"a\nb\n"), vec!["a", "b"]);
        assert_eq!(buf.take_partial(), None);
    }

    #[test]
    fn buffers_partial_line_across_chunks() {
        let mut buf = LineBuffer::new();
        assert_eq!(buf.push(b"hel"), Vec::<String>::new());
        assert_eq!(buf.push(b"lo\nwor"), vec!["hello"]);
        assert_eq!(buf.push(b"ld\n"), vec!["world"]);
    }

    #[test]
    fn take_partial_returns_unterminated_tail() {
        let mut buf = LineBuffer::new();
        assert_eq!(buf.push(b"done\ntail"), vec!["done"]);
        assert_eq!(buf.take_partial(), Some("tail".to_string()));
        assert_eq!(buf.take_partial(), None);
    }
}
