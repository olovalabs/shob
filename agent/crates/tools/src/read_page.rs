use std::io::{self, BufRead, BufReader, Cursor, Read};

const COMPLETE_READ_BYTES: u64 = 64 * 1024;
const IMAGE_HEADER_BYTES: u64 = 32;

/// A native `read` may stop at a page boundary without counting the file's tail.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadToolFile {
    Complete(Vec<u8>),
    Page {
        text: String,
        next_offset: Option<u64>,
    },
    OversizedFirstLine,
    OffsetPastEnd {
        total_lines: u64,
    },
}

pub(crate) fn read_tool_file(
    mut reader: impl Read,
    size_hint: u64,
    offset: u64,
    max_lines: usize,
    max_bytes: usize,
    detect_image: impl FnOnce(&[u8]) -> bool,
    check_cancel: impl Fn() -> io::Result<()>,
) -> io::Result<ReadToolFile> {
    check_cancel()?;
    let probe_bytes = if size_hint > COMPLETE_READ_BYTES {
        IMAGE_HEADER_BYTES
    } else {
        COMPLETE_READ_BYTES + 1
    };
    let mut prefix = Vec::with_capacity(size_hint.min(probe_bytes) as usize);
    reader.by_ref().take(probe_bytes).read_to_end(&mut prefix)?;
    check_cancel()?;
    if (prefix.len() as u64) < probe_bytes {
        return Ok(ReadToolFile::Complete(prefix));
    }
    if detect_image(&prefix) {
        // The full decoder must still see all bytes (e.g. to identify APNG).
        reader.read_to_end(&mut prefix)?;
        check_cancel()?;
        return Ok(ReadToolFile::Complete(prefix));
    }
    read_text_page(
        Cursor::new(prefix).chain(BufReader::new(reader)),
        offset.max(1),
        max_lines,
        max_bytes,
        check_cancel,
    )
}

fn read_text_page(
    mut reader: impl BufRead,
    offset: u64,
    max_lines: usize,
    max_bytes: usize,
    check_cancel: impl Fn() -> io::Result<()>,
) -> io::Result<ReadToolFile> {
    let mut current = 1;
    while current < offset {
        check_cancel()?;
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok(ReadToolFile::OffsetPastEnd {
                total_lines: current,
            });
        }
        let count = bytes
            .iter()
            .position(|&byte| {
                current += u64::from(byte == b'\n');
                current == offset
            })
            .map_or(bytes.len(), |newline| newline + 1);
        reader.consume(count);
    }

    let mut text = String::new();
    let mut line = Vec::new();
    let mut lines = 0;
    let mut terminated = false;
    let more = loop {
        check_cancel()?;
        let bytes = reader.fill_buf()?;
        let eof = bytes.is_empty();
        if lines == max_lines {
            break !eof;
        }
        if eof && line.is_empty() {
            if terminated && text.len() < max_bytes {
                text.push('\n');
            }
            break false;
        }
        let newline = bytes.iter().position(|&byte| byte == b'\n');
        let part = newline.unwrap_or(bytes.len());
        let separator = usize::from(lines != 0);
        let Some(budget) = max_bytes.checked_sub(text.len() + separator) else {
            break true;
        };
        if line.len() + part > budget {
            if lines == 0 {
                return Ok(ReadToolFile::OversizedFirstLine);
            }
            break true;
        }
        line.extend_from_slice(&bytes[..part]);
        reader.consume(part + usize::from(newline.is_some()));
        if newline.is_some() || eof {
            let decoded = String::from_utf8_lossy(&line);
            if decoded.len() > budget {
                if lines == 0 {
                    return Ok(ReadToolFile::OversizedFirstLine);
                }
                break true;
            }
            if separator != 0 {
                text.push('\n');
            }
            text.push_str(&decoded);
            line.clear();
            lines += 1;
            terminated = newline.is_some();
        }
    };
    Ok(ReadToolFile::Page {
        text,
        next_offset: more.then(|| offset.saturating_add(lines as u64)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn page(bytes: &[u8], offset: u64, lines: usize, bytes_limit: usize) -> ReadToolFile {
        read_text_page(
            BufReader::with_capacity(3, bytes),
            offset,
            lines,
            bytes_limit,
            || Ok(()),
        )
        .unwrap()
    }

    #[test]
    fn pages_handle_blank_lines_crlf_utf8_and_eof() {
        let bytes = b"before\n\n\xf0\x9f\x92\xa1\r\na\xffb\n";
        assert_eq!(
            page(bytes, 2, 1, 16),
            ReadToolFile::Page {
                text: String::new(),
                next_offset: Some(3)
            }
        );
        assert_eq!(
            page(bytes, 3, 2, 16),
            ReadToolFile::Page {
                text: "💡\r\na\u{fffd}b".into(),
                next_offset: None
            }
        );
        assert_eq!(
            page(bytes, 4, 10, 16),
            ReadToolFile::Page {
                text: "a\u{fffd}b\n".into(),
                next_offset: None
            }
        );
        assert_eq!(
            page(bytes, 5, 10, 16),
            ReadToolFile::Page {
                text: String::new(),
                next_offset: None
            }
        );
        assert_eq!(
            page(bytes, 6, 10, 16),
            ReadToolFile::OffsetPastEnd { total_lines: 5 }
        );
    }

    #[test]
    fn byte_limit_never_returns_partial_lines_or_skips_them() {
        let bytes = b"1234\n\xff\xff\nnext";
        assert_eq!(
            page(bytes, 1, 20, 6),
            ReadToolFile::Page {
                text: "1234".into(),
                next_offset: Some(2)
            }
        );
        assert_eq!(page(bytes, 2, 20, 5), ReadToolFile::OversizedFirstLine);
        assert_eq!(
            page(b"abcd\n\nmore", 1, 20, 5),
            ReadToolFile::Page {
                text: "abcd\n".into(),
                next_offset: Some(3)
            }
        );
        assert_eq!(
            page(b"abcdefghijklmnop\n", 1, 20, 5),
            ReadToolFile::OversizedFirstLine
        );
    }

    #[test]
    fn a_short_page_does_not_read_the_large_tail() {
        let consumed = Cell::new(0);
        let mut source = Cursor::new(b"first\nsecond\n").chain(io::repeat(b'x'));
        let reader = CountReads(&mut source, &consumed);
        let result =
            read_tool_file(reader, 100 * 1024 * 1024, 1, 2, 1024, |_| false, || Ok(())).unwrap();
        assert_eq!(
            result,
            ReadToolFile::Page {
                text: "first\nsecond".into(),
                next_offset: Some(3)
            }
        );
        assert_eq!(consumed.get(), IMAGE_HEADER_BYTES as usize);
    }

    #[test]
    fn stale_size_hint_does_not_force_a_full_text_read() {
        let consumed = Cell::new(0);
        let mut source = Cursor::new(b"first\nsecond\n").chain(io::repeat(b'x'));
        let reader = CountReads(&mut source, &consumed);
        let result = read_tool_file(reader, 1, 1, 2, 1024, |_| false, || Ok(())).unwrap();
        assert!(matches!(
            result,
            ReadToolFile::Page {
                next_offset: Some(3),
                ..
            }
        ));
        assert_eq!(consumed.get(), COMPLETE_READ_BYTES as usize + 1);
    }

    #[test]
    fn cancellation_is_checked_while_skipping_chunks() {
        let checks = Cell::new(0);
        let error = read_text_page(
            BufReader::with_capacity(16, io::repeat(b'x')),
            2,
            10,
            100,
            || {
                let count = checks.get() + 1;
                checks.set(count);
                if count == 4 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(checks.get(), 4);
    }

    struct CountReads<'a, R>(R, &'a Cell<usize>);

    impl<R: Read> Read for CountReads<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self.0.read(buf)?;
            self.1.set(self.1.get() + count);
            Ok(count)
        }
    }
}
