//! Line-anchor edits: `PUT`/`CUT` operations that address lines instead of
//! retyping them.
//!
//! Borrowed from oh-my-pi's `hashline` mode (`crates/edit/src/modes/hashline/`:
//! `[path#TAG]` headers, `PUT n.==m:` hunks, `<n`/`>n`/`<$` gap cursors), minus
//! the clipboard registers, moves and syntactic-block locators. Shob's `edit`
//! tool already validates `oldText` uniqueness and applies a diff, so the whole
//! job here is *expansion*: turn an anchor operation into the `oldText`/`newText`
//! pair the existing tool already understands.
//!
//! Two guards make this safe to expose to a model:
//!
//! - the tag must match the version this session last showed it (otherwise the
//!   model is addressing lines from a file that has since changed);
//! - the file's digest must still equal the recorded digest at expansion time,
//!   so line numbers cannot have shifted between the two checks.
//!
//! Both failures produce a message that names the *current* tag, so the model
//! can retry against the new version without a blind re-read.

use crate::token_efficiency::cache::ContentStamp;
use crate::token_efficiency::hash::Digest;

/// A line range, 1-based and inclusive, matching the numbering `read` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

/// Where an insertion goes, relative to an existing line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// Before the first line.
    Bof,
    /// After the last line.
    Eof,
    /// Immediately before this 1-based line.
    Before(usize),
    /// Immediately after this 1-based line.
    After(usize),
}

/// One anchor operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorOp {
    /// Replace the given lines with `text`.
    Put { range: LineRange, text: String },
    /// Delete the given lines.
    Cut { range: LineRange },
    /// Insert `text` at `cursor`.
    Insert { cursor: Cursor, text: String },
}

impl AnchorOp {
    /// The largest line number this op refers to, for bounds checking.
    fn max_line(&self) -> Option<usize> {
        match self {
            AnchorOp::Put { range, .. } | AnchorOp::Cut { range } => Some(range.end),
            AnchorOp::Insert { cursor: Cursor::Before(line), .. }
            | AnchorOp::Insert { cursor: Cursor::After(line), .. } => Some(*line),
            AnchorOp::Insert { cursor: Cursor::Bof | Cursor::Eof, .. } => None,
        }
    }
}

/// A parsed anchor patch for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorPatch {
    /// The tag the model read, e.g. `3f9a`.
    pub tag: String,
    pub ops: Vec<AnchorOp>,
}

/// Why an anchor patch could not be expanded. Each variant is written to be
/// shown to the model verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorError {
    /// The tag does not match what this session last delivered.
    Stale { expected: String, got: String },
    /// The file changed since it was delivered (digest mismatch).
    Moved { expected: String, got: String },
    /// The file is not in the ledger at all.
    NotDelivered { path: String },
    /// A line number is past the end of the file.
    OutOfRange { line: usize, lines: usize },
    /// `start > end`, or a zero line number.
    BadRange { range: LineRange },
    /// The patch is empty, or overlaps itself.
    Invalid { reason: String },
    /// Nothing to apply: the expansion produced no change.
    NoChange,
}

impl std::fmt::Display for AnchorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnchorError::Stale { expected, got } => write!(
                f,
                "anchor #{got} is stale for this session; the current tag is #{expected}. \
                 Re-read the file (or use the anchors from your most recent read) and retry."
            ),
            AnchorError::Moved { expected, got } => write!(
                f,
                "the file changed on disk since you read it (was #{expected}, now #{got}). \
                 Re-read it before editing."
            ),
            AnchorError::NotDelivered { path } => write!(
                f,
                "{path} has not been read in this session, so it has no anchors. \
                 Read it first, or use the `edits` form with `oldText`."
            ),
            AnchorError::OutOfRange { line, lines } => write!(
                f,
                "line {line} is past the end of the file ({lines} lines)"
            ),
            AnchorError::BadRange { range } => write!(
                f,
                "invalid line range {}.={}: ranges are 1-based and inclusive, with start <= end",
                range.start, range.end
            ),
            AnchorError::Invalid { reason } => write!(f, "invalid anchor patch: {reason}"),
            AnchorError::NoChange => write!(f, "the anchor patch produces no change to the file"),
        }
    }
}

/// One expansion result: the `oldText`/`newText` pair for the existing `edit`
/// tool, plus the new version's stamp so the ledger can advance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedEdit {
    pub old_text: String,
    pub new_text: String,
    /// Lines the edit covers, for logging.
    pub lines: LineRange,
}

/// Expand `ops` against `content` (the file's current text).
///
/// Operations are applied bottom-up so each op's line numbers still refer to the
/// file the model saw. Each op becomes its own `oldText`/`newText` pair, which is
/// exactly what `create_edit_tool` consumes; the tool re-checks uniqueness, so
/// two overlapping ops fail there with its own message rather than corrupting
/// the file here.
pub fn expand(content: &str, ops: &[AnchorOp]) -> Result<Vec<ExpandedEdit>, AnchorError> {
    if ops.is_empty() {
        return Err(AnchorError::Invalid {
            reason: "no operations".into(),
        });
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let line_count = lines.len();

    let mut ordered: Vec<&AnchorOp> = ops.iter().collect();
    // Bottom-up: the op touching the highest line applies first.
    ordered.sort_by_key(|op| std::cmp::Reverse(op.max_line().unwrap_or(0)));

    let mut edits = Vec::with_capacity(ops.len());
    for op in ordered {
        let edit = expand_one(op, &lines, line_count)?;
        edits.push(edit);
    }
    Ok(edits)
}

fn expand_one(op: &AnchorOp, lines: &[&str], line_count: usize) -> Result<ExpandedEdit, AnchorError> {
    match op {
        AnchorOp::Put { range, text } => {
            check_range(*range, line_count)?;
            let old_text = slice(lines, *range);
            Ok(ExpandedEdit {
                old_text,
                new_text: text.clone(),
                lines: *range,
            })
        }
        AnchorOp::Cut { range } => {
            check_range(*range, line_count)?;
            Ok(ExpandedEdit {
                old_text: slice(lines, *range),
                // Deleting the range's own text leaves the surrounding blank
                // line structure to the caller; an empty replacement is the
                // honest translation.
                new_text: String::new(),
                lines: *range,
            })
        }
        AnchorOp::Insert { cursor, text } => {
            // An insertion is expressed as a replacement of the anchor line by
            // "anchor line + new text" (or the reverse), so it goes through the
            // same uniqueness check as everything else.
            match cursor {
                Cursor::Bof => Ok(ExpandedEdit {
                    old_text: lines.first().copied().unwrap_or("").to_string(),
                    new_text: format!("{}\n{}", text.trim_end_matches('\n'), lines.first().copied().unwrap_or("")),
                    lines: LineRange { start: 1, end: 1 },
                }),
                Cursor::Eof => Ok(ExpandedEdit {
                    old_text: lines.last().copied().unwrap_or("").to_string(),
                    new_text: format!("{}\n{}", lines.last().copied().unwrap_or(""), text.trim_end_matches('\n')),
                    lines: LineRange { start: line_count, end: line_count },
                }),
                Cursor::Before(line) => {
                    check_line(*line, line_count)?;
                    let target = lines[line - 1];
                    Ok(ExpandedEdit {
                        old_text: target.to_string(),
                        new_text: format!("{}\n{}", text.trim_end_matches('\n'), target),
                        lines: LineRange { start: *line, end: *line },
                    })
                }
                Cursor::After(line) => {
                    check_line(*line, line_count)?;
                    let target = lines[line - 1];
                    Ok(ExpandedEdit {
                        old_text: target.to_string(),
                        new_text: format!("{}\n{}", target, text.trim_end_matches('\n')),
                        lines: LineRange { start: *line, end: *line },
                    })
                }
            }
        }
    }
}

fn check_line(line: usize, line_count: usize) -> Result<(), AnchorError> {
    if line == 0 || line > line_count {
        return Err(AnchorError::OutOfRange {
            line,
            lines: line_count,
        });
    }
    Ok(())
}

fn check_range(range: LineRange, line_count: usize) -> Result<(), AnchorError> {
    if range.start == 0 || range.end == 0 || range.start > range.end {
        return Err(AnchorError::BadRange { range });
    }
    check_line(range.end, line_count)
}

fn slice(lines: &[&str], range: LineRange) -> String {
    lines[range.start - 1..range.end].join("\n")
}

/// Verify the guard pair before expanding: the tag the model quoted must be the
/// tag this session delivered, and the file must still be that version.
pub fn verify(
    path: &str,
    patch: &AnchorPatch,
    ledger: Option<crate::token_efficiency::cache::AnchorLedger>,
    current: ContentStamp,
) -> Result<(), AnchorError> {
    let ledger = ledger.ok_or_else(|| AnchorError::NotDelivered {
        path: path.to_string(),
    })?;
    let expected = tag_of(&ledger.stamp.digest);
    if !patch.tag.eq_ignore_ascii_case(&expected) {
        return Err(AnchorError::Stale {
            expected,
            got: patch.tag.clone(),
        });
    }
    if current != ledger.stamp {
        return Err(AnchorError::Moved {
            expected,
            got: tag_of(&current.digest),
        });
    }
    Ok(())
}

/// The 4-hex tag for a digest, the same width oh-my-pi uses
/// (`HL_FILE_HASH_LENGTH = 4`).
pub fn tag_of(digest: &Digest) -> String {
    digest.tag(4)
}

// ---- parsing ------------------------------------------------------------

/// Parse a patch body:
///
/// ```text
/// PUT 12.==14:
/// new line one
/// new line two
/// CUT 30.==31
/// PUT >40:
/// inserted after line 40
/// PUT <$:
/// appended at end of file
/// ```
///
/// Header lines are recognised by keyword; everything between headers is the
/// `PUT` payload, verbatim.
pub fn parse(body: &str) -> Result<AnchorPatch, AnchorError> {
    let mut ops = Vec::new();
    let mut pending: Option<(AnchorOp, Vec<String>)> = None;

    let flush = |pending: &mut Option<(AnchorOp, Vec<String>)>, ops: &mut Vec<AnchorOp>| {
        if let Some((mut op, payload)) = pending.take() {
            if let AnchorOp::Put { text, .. } = &mut op {
                *text = payload.join("\n");
            }
            ops.push(op);
        }
    };

    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("PUT ") {
            flush(&mut pending, &mut ops);
            let rest = rest.trim_end();
            let (rest, has_payload) = match rest.strip_suffix(':') {
                Some(stripped) => (stripped.trim_end(), true),
                None => (rest, false),
            };
            let op = if has_payload {
                match parse_range(rest) {
                    Some(range) => AnchorOp::Put {
                        range,
                        text: String::new(),
                    },
                    None => AnchorOp::Insert {
                        cursor: parse_cursor(rest)?,
                        text: String::new(),
                    },
                }
            } else {
                // `PUT 12.==14` without a colon: still a replacement header.
                AnchorOp::Put {
                    range: parse_range(rest).ok_or_else(|| AnchorError::Invalid {
                        reason: format!("unrecognised PUT header {rest:?}"),
                    })?,
                    text: String::new(),
                }
            };
            pending = Some((op, Vec::new()));
        } else if let Some(rest) = line.trim_start().strip_prefix("CUT ") {
            flush(&mut pending, &mut ops);
            let range = parse_range(rest.trim()).ok_or_else(|| AnchorError::Invalid {
                reason: format!("unrecognised CUT header {rest:?}"),
            })?;
            ops.push(AnchorOp::Cut { range });
        } else if let Some((_, payload)) = pending.as_mut() {
            payload.push(line.to_string());
        } else if !line.trim().is_empty() {
            return Err(AnchorError::Invalid {
                reason: format!("content before any PUT/CUT header: {line:?}"),
            });
        }
    }
    flush(&mut pending, &mut ops);

    if ops.is_empty() {
        return Err(AnchorError::Invalid {
            reason: "no PUT or CUT operations".into(),
        });
    }
    Ok(AnchorPatch {
        // Filled in by the caller from the tool arguments.
        tag: String::new(),
        ops,
    })
}

/// `12.==14`, `12` (single line).
fn parse_range(text: &str) -> Option<LineRange> {
    let text = text.trim();
    if let Some((start, end)) = text.split_once(".==") {
        return Some(LineRange {
            start: start.trim().parse().ok()?,
            end: end.trim().parse().ok()?,
        });
    }
    if text.contains(".=") {
        // Accept the single-`=` typo rather than failing the whole patch.
        let (start, end) = text.split_once(".=")?;
        return Some(LineRange {
            start: start.trim().parse().ok()?,
            end: end.trim().parse().ok()?,
        });
    }
    text.parse().ok().map(|line: usize| LineRange {
        start: line,
        end: line,
    })
}

/// `<12`, `>12`, `<1`, `>$`.
fn parse_cursor(text: &str) -> Result<Cursor, AnchorError> {
    let text = text.trim();
    let invalid = || AnchorError::Invalid {
        reason: format!("unrecognised cursor {text:?}; expected <n, >n, <1 or >$"),
    };
    if let Some(number) = text.strip_prefix('<') {
        let number = number.trim();
        if number == "1" {
            // `<1` is "before the first line" in both readings; Before(1).
            return Ok(Cursor::Before(1));
        }
        return number.parse().map(Cursor::Before).map_err(|_| invalid());
    }
    if let Some(number) = text.strip_prefix('>') {
        let number = number.trim();
        if number == "$" {
            return Ok(Cursor::Eof);
        }
        return number.parse().map(Cursor::After).map_err(|_| invalid());
    }
    Err(invalid())
}

/// Render a file's lines with the numbering `read` uses, for the prompt hint
/// and for tests.
pub fn numbered(content: &str, start_line: usize) -> String {
    let mut out = String::new();
    for (index, line) in content.split('\n').enumerate() {
        out.push_str(&format!("{}:{line}\n", start_line + index));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "one\ntwo\nthree\nfour\nfive";

    fn stamp(content: &str) -> ContentStamp {
        ContentStamp::of_bytes(content.as_bytes())
    }

    fn ledger_for(content: &str) -> crate::token_efficiency::cache::AnchorLedger {
        let stamp = stamp(content);
        crate::token_efficiency::cache::AnchorLedger {
            tag: stamp.digest.as_bytes()[..4].try_into().unwrap(),
            stamp,
            delivered_turn: 1,
        }
    }

    #[test]
    fn put_replaces_the_named_lines_verbatim() {
        let ops = vec![AnchorOp::Put {
            range: LineRange { start: 2, end: 3 },
            text: "TWO\nTHREE".into(),
        }];
        let edits = expand(FILE, &ops).expect("expand");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old_text, "two\nthree");
        assert_eq!(edits[0].new_text, "TWO\nTHREE");
    }

    #[test]
    fn cut_removes_the_named_lines() {
        let edits = expand(FILE, &[AnchorOp::Cut { range: LineRange { start: 4, end: 5 } }])
            .expect("expand");
        assert_eq!(edits[0].old_text, "four\nfive");
        assert_eq!(edits[0].new_text, "");
    }

    #[test]
    fn inserts_keep_the_anchor_line_in_the_pre_image() {
        let after = expand(FILE, &[AnchorOp::Insert {
            cursor: Cursor::After(2),
            text: "two-and-a-half".into(),
        }])
        .expect("expand");
        assert_eq!(after[0].old_text, "two");
        assert_eq!(after[0].new_text, "two\ntwo-and-a-half");

        let before = expand(FILE, &[AnchorOp::Insert {
            cursor: Cursor::Before(2),
            text: "one-and-a-half".into(),
        }])
        .expect("expand");
        assert_eq!(before[0].old_text, "two");
        assert_eq!(before[0].new_text, "one-and-a-half\ntwo");

        let at_bof = expand(FILE, &[AnchorOp::Insert {
            cursor: Cursor::Bof,
            text: "zero".into(),
        }])
        .expect("expand");
        assert_eq!(at_bof[0].new_text, "zero\none");

        let at_eof = expand(FILE, &[AnchorOp::Insert {
            cursor: Cursor::Eof,
            text: "six".into(),
        }])
        .expect("expand");
        assert_eq!(at_eof[0].new_text, "five\nsix");
    }

    #[test]
    fn out_of_range_and_bad_ranges_are_rejected() {
        let past_end = expand(FILE, &[AnchorOp::Cut { range: LineRange { start: 4, end: 9 } }]);
        assert_eq!(past_end.unwrap_err(), AnchorError::OutOfRange { line: 9, lines: 5 });

        let inverted = expand(FILE, &[AnchorOp::Cut { range: LineRange { start: 4, end: 2 } }]);
        assert!(matches!(inverted.unwrap_err(), AnchorError::BadRange { .. }));

        let zero = expand(FILE, &[AnchorOp::Cut { range: LineRange { start: 0, end: 1 } }]);
        assert!(matches!(zero.unwrap_err(), AnchorError::BadRange { .. }));

        let zero_insert = expand(FILE, &[AnchorOp::Insert {
            cursor: Cursor::After(0),
            text: "x".into(),
        }]);
        assert_eq!(zero_insert.unwrap_err(), AnchorError::OutOfRange { line: 0, lines: 5 });
    }

    #[test]
    fn empty_patch_is_an_error() {
        assert!(matches!(expand(FILE, &[]).unwrap_err(), AnchorError::Invalid { .. }));
    }

    #[test]
    fn ops_apply_bottom_up_so_numbering_does_not_shift() {
        // Two non-overlapping edits: the lower one must be expanded against the
        // same numbering the model saw.
        let ops = vec![
            AnchorOp::Put {
                range: LineRange { start: 1, end: 1 },
                text: "ONE".into(),
            },
            AnchorOp::Put {
                range: LineRange { start: 5, end: 5 },
                text: "FIVE".into(),
            },
        ];
        let edits = expand(FILE, &ops).expect("expand");
        let five = edits.iter().find(|edit| edit.lines.start == 5).unwrap();
        assert_eq!(five.old_text, "five", "line 5 is still 'five', not shifted by the line-1 edit");
        let one = edits.iter().find(|edit| edit.lines.start == 1).unwrap();
        assert_eq!(one.old_text, "one");
    }

    // ---- guards ----------------------------------------------------------

    #[test]
    fn verify_accepts_the_delivered_version() {
        let patch = AnchorPatch {
            tag: tag_of(&stamp(FILE).digest),
            ops: vec![],
        };
        assert_eq!(
            verify("a.rs", &patch, Some(ledger_for(FILE)), stamp(FILE)),
            Ok(())
        );
    }

    #[test]
    fn verify_is_case_insensitive_on_the_tag() {
        let patch = AnchorPatch {
            tag: tag_of(&stamp(FILE).digest).to_uppercase(),
            ops: vec![],
        };
        assert!(verify("a.rs", &patch, Some(ledger_for(FILE)), stamp(FILE)).is_ok());
    }

    #[test]
    fn a_stale_tag_is_rejected_before_anything_is_read() {
        let patch = AnchorPatch {
            tag: "0000".into(),
            ops: vec![],
        };
        let error = verify("a.rs", &patch, Some(ledger_for(FILE)), stamp(FILE)).unwrap_err();
        match error {
            AnchorError::Stale { expected, got } => {
                assert_eq!(got, "0000");
                assert_eq!(expected, tag_of(&stamp(FILE).digest));
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn a_file_that_moved_under_the_anchor_is_rejected() {
        let delivered = ledger_for(FILE);
        let patch = AnchorPatch {
            tag: tag_of(&delivered.stamp.digest),
            ops: vec![],
        };
        // Tag matches what was delivered, but the bytes on disk differ.
        let error = verify("a.rs", &patch, Some(delivered), stamp("one\ntwo\nTHREE\nfour\nfive"))
            .unwrap_err();
        assert!(matches!(error, AnchorError::Moved { .. }));
    }

    #[test]
    fn an_unread_file_has_no_anchors() {
        let patch = AnchorPatch { tag: "abcd".into(), ops: vec![] };
        let error = verify("a.rs", &patch, None, stamp(FILE)).unwrap_err();
        assert!(matches!(error, AnchorError::NotDelivered { .. }));
        // The message tells the model exactly what to do.
        assert!(error.to_string().contains("Read it first"));
    }

    #[test]
    fn every_error_message_is_actionable() {
        for error in [
            AnchorError::Stale { expected: "aaaa".into(), got: "bbbb".into() },
            AnchorError::Moved { expected: "aaaa".into(), got: "bbbb".into() },
            AnchorError::NotDelivered { path: "a.rs".into() },
            AnchorError::OutOfRange { line: 9, lines: 5 },
            AnchorError::BadRange { range: LineRange { start: 4, end: 2 } },
        ] {
            let message = error.to_string();
            assert!(!message.is_empty());
            assert!(
                message.contains("re-read")
                    || message.contains("Read it")
                    || message.contains("1-based")
                    || message.contains("past the end"),
                "unhelpful message: {message}"
            );
        }
    }

    // ---- parsing ---------------------------------------------------------

    #[test]
    fn parses_put_with_payload() {
        let patch = parse("PUT 2.==3:\nTWO\nTHREE").expect("parse");
        assert_eq!(patch.ops, vec![AnchorOp::Put {
            range: LineRange { start: 2, end: 3 },
            text: "TWO\nTHREE".into(),
        }]);
    }

    #[test]
    fn parses_multiple_hunks_and_cut() {
        let patch = parse("PUT 1:\nONE\nCUT 4.==5").expect("parse");
        assert_eq!(patch.ops.len(), 2);
        assert_eq!(patch.ops[0], AnchorOp::Put {
            range: LineRange { start: 1, end: 1 },
            text: "ONE".into(),
        });
        assert_eq!(patch.ops[1], AnchorOp::Cut {
            range: LineRange { start: 4, end: 5 }
        });
    }

    #[test]
    fn parses_insertion_cursors() {
        let patch = parse("PUT >$:\nsix\nPUT <1:\nzero").expect("parse");
        assert_eq!(patch.ops[0], AnchorOp::Insert {
            cursor: Cursor::Eof,
            text: "six".into()
        });
        assert_eq!(patch.ops[1], AnchorOp::Insert {
            cursor: Cursor::Before(1),
            text: "zero".into()
        });
    }

    #[test]
    fn tolerates_the_single_equals_typo() {
        let patch = parse("PUT 2.=3:\nx").expect("parse");
        assert_eq!(patch.ops[0], AnchorOp::Put {
            range: LineRange { start: 2, end: 3 },
            text: "x".into()
        });
    }

    #[test]
    fn rejects_content_before_the_first_header() {
        let error = parse("stray text\nPUT 1:\nx").unwrap_err();
        assert!(matches!(error, AnchorError::Invalid { .. }));
    }

    #[test]
    fn rejects_an_empty_body() {
        assert!(matches!(parse("   \n\n").unwrap_err(), AnchorError::Invalid { .. }));
    }

    #[test]
    fn rejects_an_unparseable_cursor() {
        assert!(parse("PUT <nope:\nx").is_err());
    }

    #[test]
    fn parse_then_expand_end_to_end() {
        let mut patch = parse("PUT 2.==3:\nTWO\nTHREE\nCUT 5.==5").expect("parse");
        patch.tag = tag_of(&stamp(FILE).digest);
        verify("a.rs", &patch, Some(ledger_for(FILE)), stamp(FILE)).expect("verified");
        let edits = expand(FILE, &patch.ops).expect("expand");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits.iter().map(|e| e.old_text.len()).sum::<usize>(), "two\nthree".len() + "five".len());
    }

    #[test]
    fn numbered_rows_match_the_read_tool_format() {
        assert_eq!(numbered("a\nb", 1), "1:a\n2:b\n");
        assert_eq!(numbered("b\nc", 5), "5:b\n6:c\n");
    }
}
