//! Mirrors `packages/agent/src/harness/utils/truncate.ts` — head/tail line+byte
//! truncation with structured metadata. Used by the `read` tool (head) and the
//! `bash` tool's `execute_shell_with_capture` (tail).
//!
//! Line splitting is `\n`-only (NOT Rust's `str::lines()`, which also splits on
//! `\r\n`): mirror `splitLinesForCounting` exactly — `split('\n')`, drop the
//! trailing empty element iff the original ended with `'\n'`. Byte counts are
//! UTF-8 bytes (Rust `str::len()`); the JS surrogate-pair machinery collapses to
//! `s.len()`.

/// Keep at most this many lines when truncating. Mirrors `DEFAULT_MAX_LINES`.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Keep at most this many bytes when truncating. Mirrors `DEFAULT_MAX_BYTES`
/// (50 KB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Max chars per grep match line. Mirrors `GREP_MAX_LINE_LENGTH`. JS counts
/// UTF-16 code units; the Rust port counts `chars()` (scalar values), which
/// matches for BMP-only text (the common grep case).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit was the binding constraint when truncation occurred. Serializes
/// as the lowercase `"lines"`/`"bytes"` matching the TS `truncatedBy` wire field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncationLimit {
    Lines,
    Bytes,
}

/// Options for head/tail truncation. `None` fields fall back to the defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    fn resolve(self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// Mirrors TS `TruncationResult`. The structured metadata that tools attach to
/// their `details`. `truncated_by` is `None` when `!truncated`. The notice text
/// tools append to `output_text` is synthesized by the tool (not here) from
/// these fields.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncationLimit>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Tail-only edge case: the single partial first line taken when the final
    /// input line alone exceeds `max_bytes`.
    pub last_line_partial: bool,
    /// Head-only edge case: the first line alone exceeds `max_bytes` → empty
    /// output.
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl TruncationResult {
    fn not_truncated(
        content: String,
        max_lines: usize,
        max_bytes: usize,
        total_lines: usize,
    ) -> Self {
        let total_bytes = content.len();
        Self {
            output_bytes: total_bytes,
            output_lines: total_lines,
            content,
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        }
    }
}

/// Split on `\n` and drop a trailing empty element iff the content ends with
/// `\n`. Empty input → `[]`. Mirrors `splitLinesForCounting`.
pub fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        // The split produces a trailing "" after a final '\n' — drop it.
        if parts.last().map(|s| s.is_empty()).unwrap_or(false) {
            parts.pop();
        }
    }
    parts
}

/// `"NB"`, `"(N.N)KB"`, or `"(N.N)MB"`. Mirrors `formatSize`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("({:.1})KB", bytes as f64 / 1024.0)
    } else {
        format!("({:.1})MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Keep the FIRST N lines/bytes. Never returns a partial line — either whole
/// lines fit, or the first line alone exceeds the byte budget (→ empty output).
/// Mirrors `truncateHead`.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult::not_truncated(
            content.to_string(),
            max_lines,
            max_bytes,
            total_lines,
        );
    }

    // First-line check: a single oversized first line yields empty output.
    let first_line_bytes = lines.first().map(|l| l.len()).unwrap_or(0);
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncationLimit::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines_arr: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by: Option<TruncationLimit> = None;

    let upper = std::cmp::min(lines.len(), max_lines);
    for i in 0..upper {
        // +1 byte for the '\n' separator join("\n") will insert before this
        // line; the first line has no preceding separator.
        let line_bytes = lines[i].len() + if i > 0 { 1 } else { 0 };
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = Some(TruncationLimit::Bytes);
            break;
        }
        output_lines_arr.push(lines[i]);
        output_bytes_count += line_bytes;
    }

    // Post-loop correction: if the line limit was the binding constraint (we
    // stopped because `i` reached `max_lines`, not because of bytes), force
    // `lines`.
    if output_lines_arr.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = Some(TruncationLimit::Lines);
    }

    let output_content = output_lines_arr.join("\n");
    let final_output_bytes = output_content.len();
    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines_arr.len(),
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the LAST N lines/bytes. May return ONE partial first line (the beginning
/// of the last input line) only when that single line alone exceeds `max_bytes`.
/// Mirrors `truncateTail`.
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let total_bytes = content.len();
    let total_lines = content.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!content.is_empty() && !content.ends_with('\n'));
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult::not_truncated(
            content.to_string(),
            max_lines,
            max_bytes,
            total_lines,
        );
    }

    // A contiguous suffix needs no per-line allocation, insertion or recount.
    // As before, truncated output excludes the final newline delimiter.
    let text = content.strip_suffix('\n').unwrap_or(content);
    let mut start = text.len();
    let mut output_bytes = 0usize;
    let mut output_lines = 0usize;
    let mut last_line_partial = false;
    for line in text.rsplit('\n').take(max_lines) {
        let bytes = line.len() + usize::from(output_lines != 0);
        if bytes > max_bytes.saturating_sub(output_bytes) {
            if output_lines == 0 {
                let partial = trim_to_last_utf8_bytes(line, max_bytes);
                start -= partial.len();
                output_bytes = partial.len();
                output_lines = 1;
                last_line_partial = true;
            }
            break;
        }
        start -= bytes;
        output_bytes += bytes;
        output_lines += 1;
    }
    TruncationResult {
        content: text[start..].to_owned(),
        truncated: true,
        truncated_by: Some(if output_lines >= max_lines {
            TruncationLimit::Lines
        } else {
            TruncationLimit::Bytes
        }),
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the trailing ≤`max_bytes` bytes, snapped to a char boundary. Mirrors the
/// JS `truncateStringToBytesFromEnd`. Used by tail truncation's partial-line
/// case and by `shell_output.rs`'s rolling buffer.
pub fn trim_to_last_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes || max_bytes == 0 {
        if max_bytes == 0 {
            return "";
        }
        return s;
    }
    let mut start = s.len() - max_bytes;
    let bytes = s.as_bytes();
    // Advance forward past continuation bytes (10xxxxxx) until we hit a leading
    // byte boundary.
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    if start >= bytes.len() {
        return "";
    }
    // Floor to a valid char boundary (defensive — the loop above should suffice
    // for valid UTF-8, which `&str` always is).
    let mut start = start;
    while !s.is_char_boundary(start) {
        if start == 0 {
            return "";
        }
        start -= 1;
    }
    &s[start..]
}

/// Result of [`truncate_line`].
#[derive(Debug, Clone)]
pub struct TruncateLineOutput {
    pub text: String,
    pub was_truncated: bool,
}

/// Truncate a single line to `max_chars` scalar values, appending
/// `"... [truncated]"`. Mirrors `truncateLine`. Default `max_chars` =
/// `GREP_MAX_LINE_LENGTH`. The JS count is UTF-16 code units; the Rust port
/// counts `chars()` (scalar values) — equivalent for BMP-only text.
pub fn truncate_line(line: &str, max_chars: usize) -> TruncateLineOutput {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars {
        return TruncateLineOutput {
            text: line.to_string(),
            was_truncated: false,
        };
    }
    let head: String = chars.iter().take(max_chars).collect();
    TruncateLineOutput {
        text: format!("{}... [truncated]", head),
        was_truncated: true,
    }
}

pub fn truncate_line_default(line: &str) -> TruncateLineOutput {
    truncate_line(line, GREP_MAX_LINE_LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_drops_trailing_empty() {
        assert_eq!(split_lines_for_counting(""), Vec::<&str>::new());
        assert_eq!(split_lines_for_counting("a"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines_for_counting("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines_for_counting("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn format_size_tiers() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "(1.0)KB");
        assert_eq!(format_size(1536), "(1.5)KB");
        assert_eq!(format_size(1024 * 1024), "(1.0)MB");
    }

    #[test]
    fn truncate_head_under_limits_is_noop() {
        let r = truncate_head("a\nb\n", TruncationOptions::default());
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\n");
        assert_eq!(r.output_lines, 2);
    }

    #[test]
    fn truncate_head_line_limit() {
        let s: String = (0..3000)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_head(&s, TruncationOptions::default());
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncationLimit::Lines));
        assert_eq!(r.output_lines, DEFAULT_MAX_LINES);
        // First DEFAULT_MAX_LINES lines joined.
        assert!(r.content.starts_with("line0\nline1"));
    }

    #[test]
    fn truncate_head_first_line_exceeds_bytes() {
        let big: String = "a".repeat(DEFAULT_MAX_BYTES + 10);
        let r = truncate_head(&big, TruncationOptions::default());
        assert!(r.truncated);
        assert!(r.first_line_exceeds_limit);
        assert_eq!(r.truncated_by, Some(TruncationLimit::Bytes));
        assert!(r.content.is_empty());
    }

    #[test]
    fn truncate_tail_under_limits_is_noop() {
        let r = truncate_tail("a\nb\n", TruncationOptions::default());
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\n");
    }

    #[test]
    fn truncate_tail_keeps_last_n_lines() {
        let s: String = (0..3000)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&s, TruncationOptions::default());
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncationLimit::Lines));
        // The content should end with the last line.
        assert!(r.content.contains("line2999"));
    }

    #[test]
    fn truncate_tail_partial_last_line() {
        // A single line (no newlines) exceeding max_bytes → partial end.
        let big: String = "a".repeat(DEFAULT_MAX_BYTES + 100);
        let r = truncate_tail(&big, TruncationOptions::default());
        assert!(r.truncated);
        assert!(r.last_line_partial);
        assert!(r.content.len() <= DEFAULT_MAX_BYTES);
    }

    #[test]
    fn tail_byte_limit_reports_actual_rows_and_preserves_utf8_suffix() {
        let options = TruncationOptions {
            max_lines: Some(10),
            max_bytes: Some(6),
        };
        let tail = truncate_tail("old\nmiddle\nnew", options);
        assert_eq!(tail.content, "new");
        assert_eq!(tail.output_lines, 1);
        assert_eq!(tail.output_bytes, 3);
        assert_eq!(tail.truncated_by, Some(TruncationLimit::Bytes));
        let tail = truncate_tail(
            "prefix\n界界界",
            TruncationOptions {
                max_lines: Some(10),
                max_bytes: Some(4),
            },
        );
        assert_eq!(tail.content, "界");
        assert_eq!(tail.output_bytes, 3);
        assert!(tail.last_line_partial);
    }

    #[test]
    fn trim_to_last_utf8_bytes_snaps_to_char_boundary() {
        // "é" is 2 bytes in UTF-8. Keeping 3 bytes of "aéb" should yield "éb"
        // (skip the 1 continuation byte after 'a').
        let s = "aéb";
        assert_eq!(s.len(), 4); // a(1) é(2) b(1)
        let out = trim_to_last_utf8_bytes(s, 3);
        // 3 trailing bytes = "éb" — but é's first byte is a leading byte, so
        // start=1 → "éb".
        assert_eq!(out, "éb");
        assert_eq!(trim_to_last_utf8_bytes(s, 0), "");
        assert_eq!(trim_to_last_utf8_bytes(s, 10), s);
    }

    #[test]
    fn truncate_line_default_appends_marker() {
        let s: String = "x".repeat(GREP_MAX_LINE_LENGTH + 50);
        let out = truncate_line_default(&s);
        assert!(out.was_truncated);
        assert!(out.text.ends_with("... [truncated]"));
        let short = "hello";
        let out2 = truncate_line_default(short);
        assert!(!out2.was_truncated);
        assert_eq!(out2.text, short);
    }
}
