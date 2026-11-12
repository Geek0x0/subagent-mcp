//! Shared SSE byte/line reader.
//!
//! This module supplies only byte/line reading. Provider-specific framing
//! state machines (blank-line dispatch, `[DONE]`, `data:` concatenation, the
//! 32 MiB cap, empty-event rules) belong to the later adapter tasks; nothing
//! here interprets an SSE event. `LineReader` accumulates bytes from an
//! arbitrary [`std::io::Read`] into a [`bytes::BytesMut`] buffer, so a line
//! split across any number of read chunks is reconstructed unchanged.

use bytes::BytesMut;

/// Default maximum line length used by adapters that impose the 32 MiB cap
/// (responses / messages). Chat-completions passes `usize::MAX` because
/// go-openai's `ReadBytes` grows with the line and has no fixed scanner limit.
pub const SSE_MAX_LINE: usize = 32 * 1024 * 1024;

/// A byte-oriented line reader over `R`.
pub struct LineReader<R> {
    reader: R,
    buf: BytesMut,
    max_line: usize,
}

/// Failure while reading a line.
#[derive(Debug)]
pub enum LineError {
    /// The underlying reader failed.
    Io(std::io::Error),
    /// A line exceeded `max_line` bytes (excluding the newline).
    LineTooLong,
}

impl std::fmt::Display for LineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LineError::Io(e) => write!(f, "sse: read error: {e}"),
            LineError::LineTooLong => f.write_str("sse: line exceeds the maximum length"),
        }
    }
}

impl std::error::Error for LineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LineError::Io(e) => Some(e),
            LineError::LineTooLong => None,
        }
    }
}

impl From<std::io::Error> for LineError {
    fn from(e: std::io::Error) -> Self {
        LineError::Io(e)
    }
}

impl<R: std::io::Read> LineReader<R> {
    /// Build a reader. A line longer than `max_line` bytes yields
    /// [`LineError::LineTooLong`].
    pub fn new(reader: R, max_line: usize) -> Self {
        Self {
            reader,
            buf: BytesMut::new(),
            max_line,
        }
    }

    /// Return the next line, without its trailing `\n` and with a trailing `\r`
    /// stripped. Returns `None` only at EOF with no remaining bytes; a final
    /// line with no trailing newline is returned normally. Arbitrary read
    /// chunk boundaries are handled by accumulating into `self.buf`.
    pub fn read_line(&mut self) -> Result<Option<Vec<u8>>, LineError> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                if i > self.max_line {
                    return Err(LineError::LineTooLong);
                }
                let mut line = self.buf.split_to(i + 1);
                line.truncate(i);
                if line.last() == Some(&b'\r') {
                    line.truncate(i - 1);
                }
                return Ok(Some(line.to_vec()));
            }
            if self.buf.len() > self.max_line {
                return Err(LineError::LineTooLong);
            }
            let mut chunk = [0u8; 8192];
            let n = self.reader.read(&mut chunk)?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let mut line = self.buf.split_to(self.buf.len());
                if line.last() == Some(&b'\r') {
                    let trimmed = line.len() - 1;
                    line.truncate(trimmed);
                }
                return Ok(Some(line.to_vec()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A `Read` that yields at most `chunk` bytes per call, so a line is split
    /// across arbitrary (and even one-byte) boundaries.
    struct Chunked {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn lines(chunk: usize) -> Vec<Vec<u8>> {
        let data = b"data: one\n\ndata: two\r\n\n: ping\n\ndata: three";
        let reader = Chunked {
            data: data.to_vec(),
            pos: 0,
            chunk,
        };
        let mut lr = LineReader::new(reader, 1 << 20);
        let mut out = Vec::new();
        while let Some(line) = lr.read_line().unwrap() {
            out.push(line);
        }
        out
    }

    #[test]
    fn arbitrary_chunk_boundaries_preserve_lines() {
        let expected: Vec<Vec<u8>> = vec![
            b"data: one".to_vec(),
            b"".to_vec(),
            b"data: two".to_vec(),
            b"".to_vec(),
            b": ping".to_vec(),
            b"".to_vec(),
            b"data: three".to_vec(),
        ];
        for chunk in [1usize, 2, 3, 5, 7, 64] {
            assert_eq!(lines(chunk), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn crlf_is_stripped_and_final_line_without_newline_is_returned() {
        let reader = Chunked {
            data: b"a\r\nb".to_vec(),
            pos: 0,
            chunk: 1,
        };
        let mut lr = LineReader::new(reader, 1 << 20);
        assert_eq!(lr.read_line().unwrap(), Some(b"a".to_vec()));
        assert_eq!(lr.read_line().unwrap(), Some(b"b".to_vec()));
        assert_eq!(lr.read_line().unwrap(), None);
    }

    #[test]
    fn max_line_is_enforced_across_chunks() {
        let reader = Chunked {
            data: b"abcdefgh\n".to_vec(),
            pos: 0,
            chunk: 1,
        };
        let mut lr = LineReader::new(reader, 4);
        assert!(matches!(lr.read_line(), Err(LineError::LineTooLong)));
    }

    #[test]
    fn empty_input_returns_none() {
        let reader = Chunked {
            data: Vec::new(),
            pos: 0,
            chunk: 1,
        };
        let mut lr = LineReader::new(reader, 1 << 20);
        assert_eq!(lr.read_line().unwrap(), None);
    }

    #[test]
    fn io_errors_are_propagated() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
        }
        let mut lr = LineReader::new(Failing, 1 << 20);
        assert!(matches!(lr.read_line(), Err(LineError::Io(_))));
    }
}
