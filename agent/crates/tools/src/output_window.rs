//! Head+tail output windowing for long command output.
//!
//! The `bash` tool keeps a bounded *tail* of captured output (`shell_output.rs`)
//! because the end of a log usually carries the verdict. That is the right
//! default, but it has one specific failure mode: when a build or test run fails
//! and the root cause is printed near the *start* (the first failing assertion, a
//! compiler's first error, a missing dependency at the top of the log), a tail-only
//! window discards exactly the line the caller needs, and the caller re-runs the
//! command through `head` or re-reads the spool file to find it.
//!
//! This module builds the alternative for that case: a bounded **head** window
//! joined to the bounded **tail** window, with a marker stating exactly how many
//! lines and bytes were elided in the middle and where the complete log lives.
//!
//! Design constraints (deliberate):
//!
//! * **No summarisation.** Nothing is filtered, pattern-matched, reordered or
//!   rewritten. Output is only *split*, and the split is stated exactly. A caller
//!   that distrusts the marker can still open the full log.
//! * **Never authoritative over a failure.** The window is only built when the
//!   caller explicitly asks for it, and the callers pass a *recorded* exit code —
//!   nothing here infers success or failure from the text. A run that cannot be
//!   classified keeps the plain tail, exactly as before.
//! * **Never longer than the raw output it replaces.** If the joined window plus
//!   marker and notice would not be strictly smaller than the total output, the
//!   caller keeps its existing text. This is the size invariant a compression step
//!   needs, applied to the one quantity this module controls.
//! * **Pure and allocation-light.** `truncate_head` over the prefix, `truncate_tail`
//!   over the suffix, then one join. No filesystem, process or network access, no
//!   second store of output, and nothing here runs on the streaming hot path: the
//!   capture layer hands over two bounded buffers when the command ends.

use crate::truncate::{
    format_size, truncate_head, truncate_tail, TruncationOptions, TruncationResult,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// Divisor applied to the byte and line budgets to size the head window, so the
/// tail — which usually carries the verdict — keeps the larger share.
///
/// Eight, not two: the display budget is 50 KB of tail, and a head window is a
/// *pointer* to where the cause usually is, not a second full log. At 1/8 a
/// truncated result grows by at most ~6 KB (13%), which bounds the context cost of
/// the whole feature to one small, fixed increment.
pub const DEFAULT_HEAD_SHARE: usize = 8;

/// Totals for the *complete* output, which may be far larger than the retained
/// head and tail windows.
///
/// Line totals are exact whenever the capture layer counts them as it streams.
/// Byte totals are exact too, but the retained buffers are only the first ~6 KB and
/// last ~100 KB, so `bytes - kept` is an upper bound on the elided middle rather
/// than a measurement of it.
#[derive(Debug, Clone, Copy)]
pub struct OutputTotals {
    pub lines: usize,
    pub bytes: usize,
}

impl OutputTotals {
    pub fn new(lines: usize, bytes: usize) -> Self {
        Self { lines, bytes }
    }
}

/// One rendered head+tail window.
#[derive(Debug, Clone)]
pub struct OutputWindow {
    /// Head window, marker, tail window and trailing notice, joined by newlines.
    pub content: String,
    /// Lines kept in the head window.
    pub head_lines: usize,
    /// Lines kept in the tail window.
    pub tail_lines: usize,
    /// Lines elided between the two windows (exact).
    pub elided_lines: usize,
    /// Bytes elided between the two windows: exact when the caller passes exact
    /// totals (the capture path does), otherwise an upper bound.
    pub elided_bytes: usize,
    /// Bytes occupied by `content`, marker and notice included.
    pub output_bytes: usize,
}

/// Build a head+tail window from a retained head buffer and a retained tail
/// buffer.
///
/// Returns `None` — meaning "keep whatever the caller already had" — when:
/// * either buffer is empty or has no complete lines,
/// * the totals do not actually exceed the display limits,
/// * the tail's last line alone exceeds the byte budget (a partial-line notice is
///   the better message, and no budget is left for a head),
/// * the windows would overlap or cover everything (nothing to elide), or
/// * the joined window + marker would not be strictly smaller than the totals.
pub fn build_window(
    head_src: &str,
    tail_src: &str,
    totals: OutputTotals,
    options: TruncationOptions,
    head_share: usize,
    full_output_hint: Option<&str>,
) -> Option<OutputWindow> {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if totals.lines <= max_lines && totals.bytes <= max_bytes {
        return None;
    }
    if head_src.is_empty() || tail_src.is_empty() {
        return None;
    }
    let share = head_share.max(1);
    let head = truncate_head(
        head_src,
        TruncationOptions {
            max_lines: Some((max_lines / share).max(1)),
            max_bytes: Some((max_bytes / share).max(1)),
        },
    );
    // `head_src` is a bounded prefix, so `truncate_head` may report "not
    // truncated" while still being a strict prefix of the total output. Either way
    // its content is exactly the leading window, so only emptiness disqualifies it.
    if head.content.is_empty() || head.first_line_exceeds_limit {
        return None;
    }
    let tail = truncate_tail(tail_src, options);
    if tail.output_lines == 0 || tail.last_line_partial {
        return None;
    }
    let head_lines = head.output_lines;
    let tail_lines = tail.output_lines;
    if head_lines + tail_lines >= totals.lines {
        return None;
    }
    let elided_lines = totals.lines - head_lines - tail_lines;
    let kept_bytes = head.output_bytes + tail.output_bytes;
    let elided_bytes = totals.bytes.saturating_sub(kept_bytes);
    let hint = full_output_hint.unwrap_or("?");
    // The line count is exact. The byte figure would only be an upper bound here
    // (the retained windows are far smaller than the elided middle), and an
    // approximate number presented next to an exact one invites misreading, so it is
    // stated as a bound rather than dropped.
    // `format_size` already renders its own parentheses (`(138.7)KB`), so the
    // marker says "at least" rather than wrapping it in a second pair.
    let marker = format!(
        "[... {elided_lines} lines elided, at least {} ...]",
        format_size(elided_bytes)
    );
    let notice = format!("[Full output: {hint}]");
    let content_len =
        head.content.len() + 1 + marker.len() + 1 + tail.content.len() + 2 + notice.len();
    // Never worse: refuse to emit a window at least as large as the full log.
    if content_len >= totals.bytes {
        return None;
    }
    let mut content = String::with_capacity(content_len);
    content.push_str(&head.content);
    content.push('\n');
    content.push_str(&marker);
    content.push('\n');
    content.push_str(&tail.content);
    content.push_str("\n\n");
    content.push_str(&notice);
    Some(OutputWindow {
        output_bytes: content.len(),
        content,
        head_lines,
        tail_lines,
        elided_lines,
        elided_bytes,
    })
}

/// [`build_window`] over a single complete buffer (used where the whole output is
/// already in memory).
pub fn build_output_window(
    content: &str,
    options: TruncationOptions,
    head_share: usize,
    full_output_hint: Option<&str>,
) -> Option<OutputWindow> {
    let totals = OutputTotals {
        lines: line_count(content),
        bytes: content.len(),
    };
    build_window(content, content, totals, options, head_share, full_output_hint)
}

/// `\n`-count lines, counting a non-terminated final line. Matches
/// `truncate_tail`'s counting so the elided arithmetic lines up.
fn line_count(content: &str) -> usize {
    content.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!content.is_empty() && !content.ends_with('\n'))
}

/// Truncation metadata for a head+tail window, suitable for a tool's
/// `details.truncation`.
///
/// `total_*` are the complete-output totals; the reported `output_lines`/
/// `output_bytes` describe the window itself, so a consumer reading `details` sees
/// the same numbers the marker states.
pub fn window_truncation(
    window: &OutputWindow,
    totals: OutputTotals,
    options: TruncationOptions,
) -> TruncationResult {
    TruncationResult {
        content: window.content.clone(),
        truncated: true,
        // Report the limit that was actually binding, matching `truncate_tail`'s
        // convention, so `details` never disagrees with the reason given.
        truncated_by: Some(if totals.bytes > options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES) {
            crate::truncate::TruncationLimit::Bytes
        } else {
            crate::truncate::TruncationLimit::Lines
        }),
        total_lines: totals.lines,
        total_bytes: totals.bytes,
        output_lines: window.head_lines + window.tail_lines,
        output_bytes: window.output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines: options.max_lines.unwrap_or(DEFAULT_MAX_LINES),
        max_bytes: options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `L000123`-style lines so a test can locate any position exactly.
    fn numbered(count: usize) -> String {
        (0..count)
            .map(|i| format!("L{i:06}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn totals_of(content: &str) -> OutputTotals {
        OutputTotals {
            lines: line_count(content),
            bytes: content.len(),
        }
    }

    #[test]
    fn short_output_is_not_windowed() {
        let s = numbered(10);
        assert!(build_output_window(&s, TruncationOptions::default(), 4, None).is_none());
    }

    #[test]
    fn empty_head_or_tail_is_not_windowed() {
        let s = numbered(20_000);
        let totals = totals_of(&s);
        let opts = TruncationOptions::default();
        assert!(build_window("", &s, totals, opts, 4, None).is_none());
        assert!(build_window(&s, "", totals, opts, 4, None).is_none());
    }

    #[test]
    fn window_keeps_first_and_last_lines_and_counts_the_gap() {
        let total = 20_000;
        let s = numbered(total);
        let window = build_output_window(&s, TruncationOptions::default(), 4, Some("/tmp/o.log"))
            .expect("truncated output must window");
        // Both ends survive: the failure at the top, the verdict at the bottom.
        assert_eq!(window.content.lines().next(), Some("L000000"));
        assert!(window.content.contains("L019999"));
        // The elided count is exact and stated, so nothing is hidden silently.
        assert_eq!(
            window.elided_lines,
            total - window.head_lines - window.tail_lines
        );
        let marker_prefix = format!("[... {} lines elided, at least (", window.elided_lines);
        assert!(window.content.contains(&marker_prefix));
        assert!(window.content.contains("[Full output: /tmp/o.log]"));
        assert!(window.head_lines > 0 && window.tail_lines > 0);
        assert!(window.head_lines < window.tail_lines);
        // Metadata agrees with the rendered text.
        let truncation = window_truncation(&window, totals_of(&s), TruncationOptions::default());
        assert!(truncation.truncated);
        assert_eq!(truncation.output_bytes, window.content.len());
        assert_eq!(truncation.output_lines, window.head_lines + window.tail_lines);
        assert_eq!(truncation.total_lines, total);
    }

    #[test]
    fn elided_line_count_is_recoverable_from_the_totals() {
        let s = numbered(5_000);
        let window = build_output_window(&s, TruncationOptions::default(), 4, None).unwrap();
        let accounted = window.head_lines + window.elided_lines + window.tail_lines;
        assert_eq!(accounted, 5_000);
        // The head window's last line and the tail window's first line really are
        // `head_lines` and `total - tail_lines` respectively.
        let mut lines = window.content.lines();
        let last_head = lines.nth(window.head_lines - 1).unwrap();
        let last_head_expected = format!("L{:06}", window.head_lines - 1);
        assert_eq!(last_head, last_head_expected);
        let first_tail = format!("L{:06}", 5_000 - window.tail_lines);
        assert!(window.content.contains(&first_tail));
    }

    #[test]
    fn window_is_always_smaller_than_the_raw_output() {
        for total in [5_000, 20_000] {
            let s = numbered(total);
            let window = build_output_window(&s, TruncationOptions::default(), 4, None)
                .expect("truncated output must window");
            assert!(
                window.content.len() < s.len(),
                "window {} >= raw {} for {total} lines",
                window.content.len(),
                s.len()
            );
            // Never worse also means bounded: the tail budget plus the head share
            // plus the marker/notice it has to carry.
            let ceiling = DEFAULT_MAX_BYTES + DEFAULT_MAX_BYTES / DEFAULT_HEAD_SHARE + 512;
            assert!(window.content.len() <= ceiling);
        }
    }

    #[test]
    fn barely_truncated_output_keeps_the_caller_text() {
        // Just over the line limit: head + tail + marker would not be strictly
        // smaller than the raw text, so the guard keeps the original output.
        let s = numbered(DEFAULT_MAX_LINES + 100);
        assert!(build_output_window(&s, TruncationOptions::default(), 4, None).is_none());
    }

    #[test]
    fn head_share_controls_the_split() {
        let s = numbered(20_000);
        let mut previous_head = usize::MAX;
        for share in [2usize, 4, 8] {
            let window = build_output_window(&s, TruncationOptions::default(), share, None)
                .expect("truncated output must window");
            assert!(window.head_lines > 0);
            assert!(window.head_lines < window.tail_lines);
            // A larger divisor means a smaller head window.
            assert!(window.head_lines <= previous_head);
            previous_head = window.head_lines;
        }
    }

    #[test]
    fn head_share_zero_is_treated_as_one() {
        let s = numbered(20_000);
        let window = build_output_window(&s, TruncationOptions::default(), 0, None).unwrap();
        assert!(window.head_lines > 0);
        assert!(window.head_lines <= window.tail_lines);
    }

    #[test]
    fn missing_hint_still_produces_a_window() {
        let s = numbered(20_000);
        let window = build_output_window(&s, TruncationOptions::default(), 4, None).unwrap();
        assert!(window.content.contains("[Full output: ?]"));
    }

    #[test]
    fn single_oversized_last_line_is_not_windowed() {
        // If the final line alone exceeds the byte budget, a partial-line notice is
        // the better message and no budget remains for a head window.
        let tail = format!("head\n{}", "x".repeat(DEFAULT_MAX_BYTES + 16));
        assert!(build_output_window(&tail, TruncationOptions::default(), 4, None).is_none());
    }

    #[test]
    fn multibyte_output_is_split_on_char_boundaries() {
        let s = (0..20_000)
            .map(|i| format!("L{i:06}:界"))
            .collect::<Vec<_>>()
            .join("\n");
        let window = build_output_window(&s, TruncationOptions::default(), 4, None)
            .expect("truncated output must window");
        assert!(window.content.contains('界'));
        assert_eq!(window.content.lines().next(), Some("L000000:界"));
    }

    #[test]
    fn bounded_prefix_is_not_mistaken_for_the_whole_output() {
        // The capture path retains only a prefix; `truncate_head` may report "not
        // truncated" for it while the totals still exceed the limits. The window
        // must still be built, with the elided count measured against the totals.
        let head: String = (0..200)
            .map(|i| format!("L{i:06}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail: String = (19_800..20_000)
            .map(|i| format!("L{i:06}"))
            .collect::<Vec<_>>()
            .join("\n");
        let totals = OutputTotals {
            lines: 20_000,
            bytes: 20_000 * 8,
        };
        let window = build_window(
            &head,
            &tail,
            totals,
            TruncationOptions::default(),
            4,
            Some("/tmp/o.log"),
        )
        .expect("bounded prefix must still window");
        assert_eq!(window.head_lines, 200);
        assert_eq!(window.tail_lines, 200);
        assert_eq!(window.elided_lines, 20_000 - 200 - 200);
        assert!(window.content.contains("L019999"));
    }
}
