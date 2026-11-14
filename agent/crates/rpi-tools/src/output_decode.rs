//! Incremental text decoding for child-process output.
//!
//! Output arrives in arbitrary read-sized chunks, so a multi-byte character can
//! straddle two reads; decoding each chunk on its own turns it into `U+FFFD`.
//! [`StreamDecoder`] retains incomplete characters across reads.
//!
//! With a non-UTF-8 (or not yet resolved) fallback, only the ASCII prefix of
//! each segment is emitted immediately. From its first non-ASCII byte onward,
//! all bytes are held until `\n`, `\r`, end of stream, or the pending limit is
//! exceeded. At that point valid UTF-8 wins; otherwise the whole pending span
//! uses the fallback. ASCII after a non-ASCII byte must also be held: it may be
//! a legacy encoding's trailing byte. Both `\n` and `\r` are safe boundaries
//! for UTF-8 and legacy CJK encodings; `\r` also flushes progress displays.
//!
//! The limit bounds buffering for long lines without delimiters. Once exceeded,
//! a UTF-8 character cut by the limit is completed (at most 3 more bytes) before
//! deciding, and the decision applies until the next `\n` or `\r`, with
//! incomplete characters carried by the chosen decoder. Decisions use a fixed byte count,
//! independent of read boundaries. A known UTF-8 fallback (the non-Windows
//! default) has no ambiguity and streams immediately, retaining only incomplete
//! trailing characters.
//!
//! Windows resolves its console code page lazily, only when fallback is needed;
//! UTF-8 and console-encoded output can coexist in one command's output.
//! Async consumers use `push_async` / `finish_async`: the first-needed query is
//! coalesced on the blocking pool, never on a pipe reader's runtime thread.

use encoding_rs::{Decoder, Encoding};

// Bound memory and latency for undelimited output, while allowing ordinary
// non-ASCII lines to be classified as a whole. Decide at LIMIT + 1 bytes, or up
// to 3 bytes later when that cuts a UTF-8 character.
const PENDING_LIMIT: usize = 64 * 1024;

/// Decodes one output stream chunk by chunk. Create one per stream.
pub struct StreamDecoder {
    /// None means the Windows console encoding has not been queried yet.
    fallback: Option<&'static Encoding>,
    /// Undecided bytes starting at the segment's first non-ASCII byte.
    pending: Vec<u8>,
    /// The committed decoder, including UTF-8 when selected at the limit.
    /// It carries incomplete characters until the next read or segment end.
    legacy: Option<Decoder>,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// Decoder whose fallback is the platform's console encoding.
    pub fn new() -> Self {
        Self {
            fallback: if cfg!(windows) {
                None
            } else {
                Some(encoding_rs::UTF_8)
            },
            pending: Vec::new(),
            legacy: None,
        }
    }

    /// Decoder that decodes lines which are not UTF-8 in `fallback`.
    pub fn with_fallback(fallback: &'static Encoding) -> Self {
        Self {
            fallback: Some(fallback),
            pending: Vec::new(),
            legacy: None,
        }
    }

    /// Emit the unambiguous prefix, holding undecided non-ASCII spans as above.
    pub fn push(&mut self, mut bytes: &[u8]) -> String {
        let mut out = String::new();
        while let Some((consumed, last)) = self.push_into(bytes, &mut out) {
            self.fallback = Some(console_encoding());
            self.commit_pending(last, &mut out);
            bytes = &bytes[consumed..];
        }
        out
    }

    /// Like `push`, but first-needed console discovery runs off the runtime.
    /// ASCII and valid UTF-8 never wait for or start a console probe.
    pub async fn push_async(&mut self, mut bytes: &[u8]) -> Result<String, tokio::task::JoinError> {
        let mut out = String::new();
        while let Some((consumed, last)) = self.push_into(bytes, &mut out) {
            self.fallback = Some(console_encoding_async().await?);
            self.commit_pending(last, &mut out);
            bytes = &bytes[consumed..];
        }
        Ok(out)
    }

    // Pause exactly at the first span that needs an unknown fallback. Pending
    // bytes and the segment boundary remain intact across asynchronous discovery.
    fn push_into(&mut self, mut bytes: &[u8], out: &mut String) -> Option<(usize, bool)> {
        let input_len = bytes.len();
        while !bytes.is_empty() {
            let (mut line, ends_line) = match bytes.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(end) => (&bytes[..=end], true),
                None => (bytes, false),
            };
            if self.legacy.is_none() && self.fallback == Some(encoding_rs::UTF_8) {
                self.legacy = Some(encoding_rs::UTF_8.new_decoder_without_bom_handling());
            }
            if let Some(decoder) = self.legacy.as_mut() {
                decode_legacy(decoder, line, ends_line, out);
                bytes = &bytes[line.len()..];
                if ends_line {
                    self.legacy = None;
                }
                continue;
            }
            if self.pending.is_empty() {
                let ascii = line
                    .iter()
                    .position(|b| !b.is_ascii())
                    .unwrap_or(line.len());
                out.push_str(std::str::from_utf8(&line[..ascii]).unwrap_or_default());
                bytes = &bytes[ascii..];
                line = &line[ascii..];
                if line.is_empty() {
                    continue;
                }
            }
            // Past the limit, take one byte at a time until UTF-8 validity is
            // decided, so the decision point does not depend on reads.
            let room = (PENDING_LIMIT + 1)
                .saturating_sub(self.pending.len())
                .max(1);
            let count = line.len().min(room);
            self.pending.extend_from_slice(&line[..count]);
            bytes = &bytes[count..];
            let last = ends_line && count == line.len();
            if last || (self.pending.len() > PENDING_LIMIT && utf8_decided(&self.pending)) {
                if self.fallback.is_none() && std::str::from_utf8(&self.pending).is_err() {
                    return Some((input_len - bytes.len(), last));
                }
                self.commit_pending(last, out);
            }
        }
        None
    }

    fn commit_pending(&mut self, last: bool, out: &mut String) {
        if self.pending.is_empty() {
            return;
        }
        let encoding = if std::str::from_utf8(&self.pending).is_ok() {
            encoding_rs::UTF_8
        } else {
            *self.fallback.get_or_insert_with(console_encoding)
        };
        let mut decoder = encoding.new_decoder_without_bom_handling();
        decode_legacy(&mut decoder, &self.pending, last, out);
        self.pending.clear();
        if !last {
            self.legacy = Some(decoder);
        }
    }

    /// Decode whatever is still held back; call once at end of stream.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if let Some(mut decoder) = self.legacy.take() {
            decode_legacy(&mut decoder, &[], true, &mut out);
        } else {
            self.commit_pending(true, &mut out);
        }
        out
    }

    /// Finalize already-read bytes without blocking on console discovery.
    pub async fn finish_async(&mut self) -> Result<String, tokio::task::JoinError> {
        if self.legacy.is_none()
            && self.fallback.is_none()
            && std::str::from_utf8(&self.pending).is_err()
        {
            self.fallback = Some(console_encoding_async().await?);
        }
        Ok(self.finish())
    }
}

/// Whether `bytes` is valid UTF-8 or contains an invalid sequence, as opposed
/// to valid UTF-8 cut inside its last character (at most 3 bytes).
fn utf8_decided(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).map_or_else(|err| err.error_len().is_some(), |_| true)
}

/// Decode a whole buffer at once (e.g. a probe's captured stdout).
pub fn decode_output(bytes: &[u8]) -> String {
    let mut decoder = StreamDecoder::new();
    let mut out = decoder.push(bytes);
    out.push_str(&decoder.finish());
    out
}

/// Nonblocking counterpart for asynchronous consumers such as shell probes.
pub async fn decode_output_async(bytes: &[u8]) -> Result<String, tokio::task::JoinError> {
    let mut decoder = StreamDecoder::new();
    let mut out = decoder.push_async(bytes).await?;
    out.push_str(&decoder.finish_async().await?);
    Ok(out)
}

fn decode_legacy(decoder: &mut Decoder, bytes: &[u8], last: bool, out: &mut String) {
    let room = decoder
        .max_utf8_buffer_length(bytes.len())
        .unwrap_or(bytes.len() * 3 + 16);
    out.reserve(room);
    let _ = decoder.decode_to_string(bytes, out, last);
}

#[cfg(windows)]
// Unlike a forcing lazy initializer, this permits a nonblocking cache peek
// before deciding whether asynchronous discovery is needed.
static CONSOLE_ENCODING: std::sync::OnceLock<&'static Encoding> = std::sync::OnceLock::new();

/// Coalesce async discovery while sharing the synchronous decoder's once cache.
async fn console_encoding_async() -> Result<&'static Encoding, tokio::task::JoinError> {
    #[cfg(windows)]
    {
        if let Some(encoding) = CONSOLE_ENCODING.get() {
            return Ok(*encoding);
        }
        static ENCODING: tokio::sync::OnceCell<&'static Encoding> =
            tokio::sync::OnceCell::const_new();
        return ENCODING
            .get_or_try_init(|| async { tokio::task::spawn_blocking(console_encoding).await })
            .await
            .copied();
    }
    #[cfg(not(windows))]
    Ok(encoding_rs::UTF_8)
}

/// The encoding console programs write in.
#[cfg(windows)]
pub fn console_encoding() -> &'static Encoding {
    *CONSOLE_ENCODING.get_or_init(|| {
        console_code_page()
            .and_then(codepage::to_encoding)
            .unwrap_or(encoding_rs::UTF_8)
    })
}

/// The encoding console programs write in.
#[cfg(not(windows))]
pub fn console_encoding() -> &'static Encoding {
    encoding_rs::UTF_8
}

/// The console output code page, read from `chcp` (a child inherits our
/// console, or gets a new one in the OEM code page — the same page our own
/// children write in). Spawning keeps this crate free of `unsafe` FFI, as
/// `taskkill` does for process trees; it runs once, when a pending span first
/// requires fallback (including an incomplete UTF-8 span at EOF or the limit).
#[cfg(windows)]
fn console_code_page() -> Option<u16> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let out = std::process::Command::new("cmd")
        .args(["/d", "/c", "chcp"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_chcp(&out.stdout)
}

/// The number at the end of `chcp`'s localized "Active code page: 936".
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_chcp(stdout: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(stdout);
    let digits: String = text
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_decoding_preserves_mixed_segments_and_partial_characters() {
        let bytes = b"ascii\n\xE4\xB8\xAD\n\xC2\xA9\xD6\xD0\r\xE4\xB8";
        let mut expected_decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        let expected = decode_in_reads(&mut expected_decoder, bytes, bytes.len());
        for read in [1, 2, 3, 8192] {
            let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
            let mut actual = String::new();
            for chunk in bytes.chunks(read) {
                actual.push_str(&decoder.push_async(chunk).await.unwrap());
            }
            actual.push_str(&decoder.finish_async().await.unwrap());
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn ascii_and_valid_utf8_do_not_require_a_tokio_runtime() {
        use futures::FutureExt;
        let mut decoder = StreamDecoder {
            fallback: None,
            pending: Vec::new(),
            legacy: None,
        };
        let output = async {
            let mut output = decoder.push_async(b"ascii ").await.unwrap();
            output.push_str(&decoder.push_async("中文\n".as_bytes()).await.unwrap());
            output.push_str(&decoder.finish_async().await.unwrap());
            output
        }
        .now_or_never()
        .expect("ASCII and valid UTF-8 must decode immediately");
        assert_eq!(output, "ascii 中文\n");
    }

    fn decode_in_reads(decoder: &mut StreamDecoder, bytes: &[u8], read: usize) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(read) {
            out.push_str(&decoder.push(chunk));
        }
        out.push_str(&decoder.finish());
        out
    }

    fn decode_at_splits(bytes: &[u8], splits: &[usize]) -> String {
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        let mut out = String::new();
        let mut start = 0;
        for end in splits.iter().copied().chain(std::iter::once(bytes.len())) {
            out.push_str(&decoder.push(&bytes[start..end]));
            start = end;
        }
        out.push_str(&decoder.finish());
        out
    }

    #[test]
    fn decoding_is_independent_of_read_boundaries() {
        let ambiguous = b"\xC2\xA9\xD6\xD0\n";
        let mut mixed = "前 UTF-8\n".as_bytes().to_vec();
        mixed.extend_from_slice(b"\xC2\xA9\xD6\xD0\nprogress \xD6\xD0\r");
        mixed.extend_from_slice("后\r".as_bytes());
        mixed.extend_from_slice(b"\xE4\xB8\xAD\xE4\xB8");
        for bytes in [ambiguous.as_slice(), mixed.as_slice()] {
            let expected = decode_at_splits(bytes, &[]);
            for first in 0..=bytes.len() {
                assert_eq!(decode_at_splits(bytes, &[first]), expected, "split {first}");
                for second in first..=bytes.len() {
                    assert_eq!(
                        decode_at_splits(bytes, &[first, second]),
                        expected,
                        "splits {first}, {second}"
                    );
                }
            }
        }
        assert_eq!(decode_at_splits(ambiguous, &[]), "漏中\n");
    }

    #[test]
    fn non_ascii_span_waits_for_a_segment_boundary() {
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(decoder.push(b"progress \xC2\xA9"), "progress ");
        assert_eq!(decoder.push(b"\xD6\xD0"), "");
        assert_eq!(decoder.push(b"\r"), "漏中\r");
        // 0x40 is ASCII, but here it is the second byte of a GBK character.
        assert_eq!(decoder.push(b"\x81@"), "");
        assert_eq!(decoder.push(b"\n"), "丂\n");
        assert_eq!(decoder.push("中文".as_bytes()), "");
        assert_eq!(decoder.finish(), "中文");
    }

    #[test]
    fn utf8_fallback_streams_complete_characters_immediately() {
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::UTF_8);
        assert_eq!(decoder.push("中文".as_bytes()), "中文");
        assert_eq!(decoder.push(b"\xE4\xB8"), "");
        assert_eq!(decoder.finish(), "�");
    }

    #[test]
    fn pending_limit_commits_the_encoding_until_the_next_boundary() {
        for prefix in [b"\xC2\xA9".as_slice(), b"\xC2\xA9\xD6\xD0".as_slice()] {
            let mut bytes = prefix.to_vec();
            bytes.resize(PENDING_LIMIT + 1, b'a');
            bytes.extend_from_slice(b"\xD6\xD0\n\xD6\xD0\n");
            let expected = decode_at_splits(&bytes, &[]);
            let (head, _, _) = encoding_rs::GBK.decode(prefix);
            if prefix.len() == 2 {
                assert!(expected.starts_with('©'));
                assert!(expected.ends_with("��\n中\n"));
            } else {
                assert!(expected.starts_with(head.as_ref()));
                assert!(expected.ends_with("中\n中\n"));
            }
            for split in [
                1,
                2,
                PENDING_LIMIT - 1,
                PENDING_LIMIT,
                PENDING_LIMIT + 1,
                PENDING_LIMIT + 2,
            ] {
                assert_eq!(
                    decode_at_splits(&bytes, &[split]),
                    expected,
                    "split {split}"
                );
            }
            let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
            assert_eq!(decoder.push(&bytes[..PENDING_LIMIT]), "");
            assert!(!decoder
                .push(&bytes[PENDING_LIMIT..PENDING_LIMIT + 1])
                .is_empty());
        }
    }

    #[test]
    fn pending_limit_keeps_characters_cut_by_the_limit() {
        for ch in ["é", "中", "😀"] {
            for cut in 1..ch.len() {
                // Pending starts at "é"; `ch` starts `cut` bytes before the limit.
                let mut text = format!("é{}", "a".repeat(PENDING_LIMIT - 1 - cut));
                text.push_str(&ch.repeat(3));
                text.push('\n');
                let bytes = text.as_bytes();
                for splits in [vec![], vec![PENDING_LIMIT], vec![PENDING_LIMIT + 1]] {
                    assert_eq!(decode_at_splits(bytes, &splits), text, "{ch} cut {cut}");
                }
                let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
                assert_eq!(decode_in_reads(&mut decoder, bytes, 8192), text);
            }
        }
        // A GBK lead byte at the limit that also starts a UTF-8 sequence.
        let mut bytes = b"\xC2\xA9".to_vec();
        bytes.resize(PENDING_LIMIT, b'a');
        bytes.extend_from_slice(b"\xD6\xD0\n");
        assert!(decode_at_splits(&bytes, &[]).ends_with("a中\n"));
    }

    #[test]
    fn pending_limit_emits_as_soon_as_the_cut_character_is_complete() {
        for ch in ["é", "中", "😀"] {
            for cut in 1..ch.len() {
                // Start pending at a non-ASCII byte, then put exactly `cut`
                // bytes of the final character inside the decision window.
                let text = format!("é{}{ch}", "a".repeat(PENDING_LIMIT - 1 - cut));
                let bytes = text.as_bytes();
                let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
                assert_eq!(decoder.push(&bytes[..PENDING_LIMIT + 1]), "");
                for end in PENDING_LIMIT + 2..bytes.len() {
                    assert_eq!(decoder.push(&bytes[end - 1..end]), "");
                }
                // No delimiter or EOF: completing the character must release
                // the pending span immediately, not buffer the rest of a line.
                assert_eq!(decoder.push(&bytes[bytes.len() - 1..]), text);
                assert_eq!(decoder.push(b"tail"), "tail");
                assert_eq!(decoder.finish(), "");
            }
        }
    }

    #[test]
    fn utf8_character_split_across_reads_survives() {
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(
            decode_in_reads(&mut decoder, "中文 ok\n".as_bytes(), 1),
            "中文 ok\n"
        );
    }

    #[test]
    fn gbk_character_split_across_reads_survives() {
        // "a中文" in GBK, one byte per read.
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        let bytes = [b'a', 0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_in_reads(&mut decoder, &bytes, 1), "a中文");
    }

    #[test]
    fn each_line_picks_its_own_encoding() {
        let mut bytes = "前\n".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4, b'\n']);
        bytes.extend_from_slice("后\n".as_bytes());
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(decode_in_reads(&mut decoder, &bytes, 3), "前\n中文\n后\n");
    }

    #[test]
    fn partial_line_is_emitted_before_its_newline() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push(b"progress 50%"), "progress 50%");
        assert_eq!(decoder.push(b"\n"), "\n");
    }

    #[test]
    fn chcp_output_yields_the_code_page() {
        assert_eq!(parse_chcp(b"Active code page: 936\r\n"), Some(936));
        assert_eq!(parse_chcp("活动代码页: 65001\r\n".as_bytes()), Some(65001));
    }
}
