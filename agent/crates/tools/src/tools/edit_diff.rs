//! Mirrors `packages/agent/src/harness/tools/edit-diff.ts` — the diff/fuzzy/apply
//! engine backing the `edit` tool.
//!
//! Pipeline: `detect_line_ending` → `strip_bom` → `normalize_to_lf` →
//! `apply_edits_to_normalized_content` (fuzzy find, not-found/duplicate/overlap/
//! no-change errors, line-preserving replacement when fuzzy) →
//! `restore_line_endings` → `generate_diff_string` + `generate_unified_patch`.

use similar::{udiff::unified_diff, Algorithm, TextDiff};

use crate::error::FileError;

/// Result of [`strip_bom`].
pub struct BomResult<'a> {
    pub bom: &'static str,
    pub text: &'a str,
}

/// Strip a leading U+FEFF BOM. Mirrors `stripBom` (returns the BOM string +
/// the text without it).
pub fn strip_bom(content: &str) -> BomResult<'_> {
    const BOM: &str = "\u{FEFF}";
    if let Some(rest) = content.strip_prefix(BOM) {
        BomResult {
            bom: BOM,
            text: rest,
        }
    } else {
        BomResult {
            bom: "",
            text: content,
        }
    }
}

/// Detect the dominant line ending: `\r\n` or `\n`. Mirrors `detectLineEnding`.
/// Lone `\r` is treated as `\n` (the normalizer maps both to `\n`).
pub fn detect_line_ending(content: &str) -> &'static str {
    let first_crlf = content.find("\r\n");
    let first_lf = content.find('\n');
    match (first_crlf, first_lf) {
        (None, None) => "\n",
        (Some(_), None) => "\n",
        (Some(crlf), Some(lf)) => {
            if crlf <= lf {
                "\r\n"
            } else {
                "\n"
            }
        }
        (None, Some(_)) => "\n",
    }
}

/// `\r\n` and lone `\r` → `\n`. Mirrors `normalizeToLF`.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `\n` → `ending`. Mirrors `restoreLineEndings`.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// NFKC + per-line trimEnd + smart-quote/dash/space normalization. Mirrors
/// `normalizeForFuzzyMatch`.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfkc: String = text.nfkc().collect();
    // Per-line trimEnd.
    let trimmed: String = nfkc
        .split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => out.push('-'),
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// Result of [`fuzzy_find_text`].
#[derive(Clone)]
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    /// The content to splice into (original when exact, normalized when fuzzy).
    pub content_for_replacement: String,
}

/// Find `old_text` in `content`: exact first, then fuzzy-normalized. Mirrors
/// `fuzzyFindText`.
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(idx) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: idx,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if let Some(idx) = fuzzy_content.find(&fuzzy_old) {
        return FuzzyMatchResult {
            found: true,
            index: idx,
            match_length: fuzzy_old.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        };
    }
    FuzzyMatchResult {
        found: false,
        index: usize::MAX,
        match_length: 0,
        used_fuzzy_match: false,
        content_for_replacement: content.to_string(),
    }
}

/// Count occurrences of `old_text` in `content` (both fuzzy-normalized). Mirrors
/// `countOccurrences`.
pub fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fc = normalize_for_fuzzy_match(content);
    let fo = normalize_for_fuzzy_match(old_text);
    if fo.is_empty() {
        return 0;
    }
    fc.matches(&fo).count()
}

/// A single replacement `{ old_text, new_text }`.
#[derive(Clone)]
pub struct ReplaceEdit {
    pub old_text: String,
    pub new_text: String,
}

/// A matched replacement ready to apply.
#[derive(Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

/// Result of [`apply_edits_to_normalized_content`].
#[derive(Debug)]
pub struct ApplyResult {
    pub base_content: String,
    pub new_content: String,
    pub used_fuzzy_match: bool,
}

/// Empty-`old_text` / not-found / duplicate / overlap / no-change → `Err`.
/// Mirrors `applyEditsToNormalizedContent`.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[ReplaceEdit],
    path: &str,
) -> Result<ApplyResult, FileError> {
    let total = edits.len();
    let normalized_edits: Vec<ReplaceEdit> = edits
        .iter()
        .map(|e| ReplaceEdit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    // Empty oldText check.
    for (i, e) in normalized_edits.iter().enumerate() {
        if e.old_text.is_empty() {
            return Err(FileError::new(
                crate::error::FileErrorCode::Invalid,
                empty_old_text_error(path, i, total),
            )
            .with_path(path));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|e| fuzzy_find_text(normalized_content, &e.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, e) in normalized_edits.iter().enumerate() {
        let m = fuzzy_find_text(&replacement_base_content, &e.old_text);
        if !m.found {
            return Err(FileError::new(
                crate::error::FileErrorCode::NotFound,
                not_found_error(path, i, total),
            )
            .with_path(path));
        }
        let occurrences = count_occurrences(&replacement_base_content, &e.old_text);
        if occurrences > 1 {
            return Err(FileError::new(
                crate::error::FileErrorCode::Invalid,
                format!(
                    "{}{}",
                    duplicate_error(path, i, total, occurrences),
                    duplicate_match_context(normalized_content, &e.old_text),
                ),
            )
            .with_path(path));
        }
        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: m.index,
            match_length: m.match_length,
            new_text: e.new_text.clone(),
        });
    }

    // Sort by match_index, check overlap.
    matched_edits.sort_by_key(|m| m.match_index);
    for pair in matched_edits.windows(2) {
        let prev = &pair[0];
        let cur = &pair[1];
        if prev.match_index + prev.match_length > cur.match_index {
            return Err(FileError::new(
                crate::error::FileErrorCode::Invalid,
                format!(
                    "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
                    prev.edit_index, cur.edit_index, path
                ),
            )
            .with_path(path));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )?
    } else {
        apply_replacements(&replacement_base_content, &matched_edits)
    };

    if base_content == new_content {
        return Err(FileError::new(
            crate::error::FileErrorCode::Invalid,
            no_change_error(path, total),
        )
        .with_path(path));
    }

    Ok(ApplyResult {
        base_content,
        new_content,
        used_fuzzy_match,
    })
}

/// Splice replacements in reverse match-index order. Mirrors `applyReplacements`.
fn apply_replacements(content: &str, replacements: &[MatchedEdit]) -> String {
    let mut result = content.to_string();
    let mut sorted = replacements.to_vec();
    sorted.sort_by(|a, b| b.match_index.cmp(&a.match_index));
    for r in sorted {
        let start = r.match_index;
        let end = start + r.match_length;
        if end <= result.len() {
            result.replace_range(start..end, &r.new_text);
        }
    }
    result
}

/// Preserve original (non-normalized) bytes on untouched lines. Mirrors
/// `applyReplacementsPreservingUnchangedLines`.
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, FileError> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(FileError::new(
            crate::error::FileErrorCode::Invalid,
            "Cannot preserve unchanged lines because the base content has a different line count.",
        ));
    }
    // Group replacements by line range.
    let mut sorted = replacements.to_vec();
    sorted.sort_by(|a, b| a.match_index.cmp(&b.match_index));
    let mut groups: Vec<(usize, usize, Vec<MatchedEdit>)> = Vec::new();
    for r in &sorted {
        let range =
            get_replacement_line_range(&base_lines, r.match_index, r.match_index + r.match_length);
        if let Some(last) = groups.last_mut() {
            if range.start_line < last.1 {
                last.1 = last.1.max(range.end_line);
                last.2.push(r.clone());
                continue;
            }
        }
        groups.push((range.start_line, range.end_line, vec![r.clone()]));
    }

    let mut out = String::new();
    let mut original_line_idx = 0usize;
    for (start_line, end_line, reps) in &groups {
        // Append unchanged leading lines.
        if original_line_idx < *start_line {
            out.push_str(&original_lines[original_line_idx..*start_line].join(""));
        }
        // Apply replacements to the base slice for this line group.
        let group_start_offset = base_lines[*start_line].0;
        let group_end_offset = base_lines[*end_line - 1].1;
        let group_slice = &base_content[group_start_offset..group_end_offset];
        // Offset replacements' match_index by -group_start_offset.
        let offset_reps: Vec<MatchedEdit> = reps
            .iter()
            .map(|r| MatchedEdit {
                edit_index: r.edit_index,
                match_index: r.match_index - group_start_offset,
                match_length: r.match_length,
                new_text: r.new_text.clone(),
            })
            .collect();
        out.push_str(&apply_replacements(group_slice, &offset_reps));
        original_line_idx = *end_line;
    }
    // Append remaining unchanged lines.
    if original_line_idx < original_lines.len() {
        out.push_str(&original_lines[original_line_idx..].join(""));
    }
    Ok(out)
}

/// Split content into lines, each preserving its trailing `\n` (or the final
/// line without one). Mirrors `splitLinesWithEndings`.
fn split_lines_with_endings(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for c in content.chars() {
        acc.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut acc));
        }
    }
    if !acc.is_empty() {
        out.push(acc);
    }
    out
}

/// `(start, end)` byte-offset spans per line. Mirrors `getLineSpans`.
fn get_line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (i, c) in content.char_indices() {
        if c == '\n' {
            spans.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < content.len() {
        spans.push((start, content.len()));
    }
    spans
}

struct LineRange {
    start_line: usize,
    end_line: usize,
}

/// Find the line range covering `[start_offset, end_offset)`. Mirrors
/// `getReplacementLineRange`. `end_line` is exclusive.
fn get_replacement_line_range(
    lines: &[(usize, usize)],
    start_offset: usize,
    end_offset: usize,
) -> LineRange {
    let mut start_line = 0usize;
    for (i, span) in lines.iter().enumerate() {
        if start_offset >= span.0 && start_offset < span.1 {
            start_line = i;
            break;
        }
        start_line = i + 1;
    }
    let mut end_line = start_line + 1;
    while end_line < lines.len() && lines[end_line - 1].1 < end_offset {
        end_line += 1;
    }
    LineRange {
        start_line,
        end_line,
    }
}

/// `{ diff, first_changed_line }`. Mirrors `generateDiffString`.
pub struct DiffResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

/// A display-oriented diff (NOT a unified patch) with line-number prefixes.
/// Mirrors `generateDiffString` with a 4-line context window.
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> DiffResult {
    let parts = TextDiff::from_lines(old_content, new_content);
    let ops: Vec<_> = parts.ops().to_vec();
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut out: Vec<String> = Vec::new();
    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (i, op) in ops.iter().enumerate() {
        let tag = op.tag();
        // The raw lines for this op — sliced from the op ranges directly
        // (mirrors TS `part.value.split("\n")` + trailing-empty pop).
        let mut raw: Vec<&str> = match tag {
            similar::DiffTag::Equal | similar::DiffTag::Delete => {
                old_lines[op.old_range().start..op.old_range().end].to_vec()
            }
            _ => new_lines[op.new_range().start..op.new_range().end].to_vec(),
        };
        if raw.last().map(|s| s.is_empty()).unwrap_or(false) {
            raw.pop();
        }
        let raw_len = raw.len();

        let is_added = matches!(tag, similar::DiffTag::Insert | similar::DiffTag::Replace);
        let is_removed = matches!(tag, similar::DiffTag::Delete | similar::DiffTag::Replace);

        if is_added || is_removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            // A `Replace` op is the removed old lines followed by the added new
            // lines, as jsdiff reports them; the two ranges may differ in length.
            if is_removed {
                let mut removed = &old_lines[op.old_range().start..op.old_range().end];
                if removed.last().is_some_and(|s| s.is_empty()) {
                    removed = &removed[..removed.len() - 1];
                }
                for line in removed {
                    out.push(format!(
                        "-{:>width$} {}",
                        old_line_num,
                        line,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                }
            }
            if is_added {
                for line in &raw {
                    out.push(format!(
                        "+{:>width$} {}",
                        new_line_num,
                        line,
                        width = line_num_width
                    ));
                    new_line_num += 1;
                }
            }
            last_was_change = true;
            let _ = i; // index only needed for context lookahead below
        } else {
            // Equal → context lines, show a few before/after changes.
            let next_is_change = i + 1 < ops.len()
                && matches!(
                    ops[i + 1].tag(),
                    similar::DiffTag::Insert | similar::DiffTag::Delete | similar::DiffTag::Replace
                );
            let has_leading_change = last_was_change;
            let has_trailing_change = next_is_change;

            if has_leading_change && has_trailing_change {
                if raw_len <= context_lines * 2 {
                    for line in &raw {
                        out.push(format!(
                            " {:>width$} {}",
                            old_line_num,
                            line,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading = &raw[..context_lines];
                    let trailing = &raw[raw_len - context_lines..];
                    let skipped = raw_len - leading.len() - trailing.len();
                    for line in leading {
                        out.push(format!(
                            " {:>width$} {}",
                            old_line_num,
                            line,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    out.push(format!(" {:>width$} ...", "", width = line_num_width));
                    old_line_num += skipped;
                    new_line_num += skipped;
                    for line in trailing {
                        out.push(format!(
                            " {:>width$} {}",
                            old_line_num,
                            line,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown = if raw_len <= context_lines {
                    &raw[..]
                } else {
                    &raw[..context_lines]
                };
                let skipped = raw_len - shown.len();
                for line in shown {
                    out.push(format!(
                        " {:>width$} {}",
                        old_line_num,
                        line,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                if skipped > 0 {
                    out.push(format!(" {:>width$} ...", "", width = line_num_width));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            } else if has_trailing_change {
                let skipped = raw_len.saturating_sub(context_lines);
                if skipped > 0 {
                    out.push(format!(" {:>width$} ...", "", width = line_num_width));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for line in &raw[skipped..] {
                    out.push(format!(
                        " {:>width$} {}",
                        old_line_num,
                        line,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                // Skip these context lines entirely.
                old_line_num += raw_len;
                new_line_num += raw_len;
            }
            last_was_change = false;
        }
    }

    DiffResult {
        diff: out.join("\n"),
        first_changed_line,
    }
}

/// A unified-diff patch with 4 lines of context, file headers only (no
/// timestamps). Mirrors `generateUnifiedPatch` (via the `similar` crate, which
/// is the Rust equivalent of the npm `diff` package).
pub fn generate_unified_patch(path: &str, old_content: &str, new_content: &str) -> String {
    // `similar::udiff::unified_diff(alg, old, new, context_radius, header)`
    // where `header: Option<(&str,&str)>` carries the (old_label, new_label)
    // file headers (`--- ` / `+++ `). No timestamps, matching the TS port.
    unified_diff(
        Algorithm::Myers,
        old_content,
        new_content,
        4,
        Some((path, path)),
    )
}

// --- error-message builders (mirror the TS single-vs-multi variants) ---

fn empty_old_text_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{i}].oldText must not be empty in {path}.")
    }
}

fn not_found_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!("Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines.")
    } else {
        format!("Could not find edits[{i}] in {path}. The old text must match exactly including all whitespace and newlines.")
    }
}

fn duplicate_error(path: &str, i: usize, total: usize, occurrences: usize) -> String {
    if total == 1 {
        format!("Found {occurrences} occurrences of the text in {path}. The text must be unique — include more surrounding context to disambiguate.")
    } else {
        format!("Found {occurrences} occurrences of edits[{i}] in {path}. The text must be unique — include more surrounding context to disambiguate.")
    }
}

/// Show bounded diagnostics using the same normalization as duplicate detection.
/// Line numbers refer to the original file, and snippets preserve its characters.
fn duplicate_match_context(content: &str, old_text: &str) -> String {
    const MAX_MATCHES: usize = 5;
    const MAX_LINE_CHARS: usize = 200;
    let normalized = normalize_for_fuzzy_match(content);
    let needle = normalize_for_fuzzy_match(old_text);
    if needle.is_empty() {
        return String::new();
    }
    let mut line_starts = vec![0];
    line_starts.extend(normalized.match_indices('\n').map(|(index, _)| index + 1));
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut out = String::from(
        "\nMatching locations (line labels are diagnostic; do not include them in oldText):\n",
    );
    for (number, (offset, _)) in normalized.match_indices(&needle).enumerate() {
        if number == MAX_MATCHES {
            out.push_str("[More matches omitted; use read to inspect the intended region.]\n");
            break;
        }
        let line = line_starts.partition_point(|start| *start <= offset) - 1;
        out.push_str(&format!("Match {} at line {}:\n", number + 1, line + 1));
        let start = line.saturating_sub(1);
        let end = (line + 2).min(original_lines.len());
        for (index, text) in original_lines.iter().enumerate().take(end).skip(start) {
            let snippet: String = text.chars().take(MAX_LINE_CHARS).collect();
            let suffix = if text.chars().count() > MAX_LINE_CHARS {
                " [line truncated]"
            } else {
                ""
            };
            out.push_str(&format!("{}: {snippet}{suffix}\n", index + 1));
        }
    }
    out.push_str("Read the intended region and include enough surrounding text to make oldText unique before retrying.");
    out
}

fn no_change_error(path: &str, total: usize) -> String {
    if total == 1 {
        format!("No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.")
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_to_lf_collapses_crlf_and_cr() {
        assert_eq!(normalize_to_lf("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn detect_line_ending_prefers_crlf_when_first() {
        assert_eq!(detect_line_ending("a\r\nb\nc"), "\r\n");
        assert_eq!(detect_line_ending("a\nb\r\nc"), "\n");
        assert_eq!(detect_line_ending("noslash"), "\n");
    }

    #[test]
    fn diff_string_shows_replaced_lines_old_then_new() {
        let old = "keep\nold one\nold two\nkeep\n";
        let new = "keep\nnew\nkeep\n";
        let diff = generate_diff_string(old, new, 4).diff;
        assert_eq!(diff, " 1 keep\n-2 old one\n-3 old two\n+2 new\n 4 keep");
    }

    #[test]
    fn strip_bom_removes_prefix() {
        let r = strip_bom("\u{FEFF}hello");
        assert_eq!(r.bom, "\u{FEFF}");
        assert_eq!(r.text, "hello");
        let r2 = strip_bom("hello");
        assert_eq!(r2.bom, "");
        assert_eq!(r2.text, "hello");
    }

    #[test]
    fn fuzzy_find_exact_then_smart_quotes() {
        let content = "it's a test";
        let m = fuzzy_find_text(content, "it's a test");
        assert!(m.found);
        assert!(!m.used_fuzzy_match);
        // Smart-quote variant.
        let m2 = fuzzy_find_text(content, "it\u{2019}s a test");
        assert!(m2.found);
        assert!(m2.used_fuzzy_match);
    }

    #[test]
    fn apply_edits_single_replacement() {
        let content = "line1\nline2\nline3\n";
        let edits = vec![ReplaceEdit {
            old_text: "line2".to_string(),
            new_text: "LINE2".to_string(),
        }];
        let r = apply_edits_to_normalized_content(content, &edits, "/x").unwrap();
        assert!(r.new_content.contains("LINE2"));
        assert!(!r.new_content.contains("line2"));
    }

    #[test]
    fn apply_edits_not_found_errors() {
        let content = "line1\nline2\n";
        let edits = vec![ReplaceEdit {
            old_text: "nope".to_string(),
            new_text: "yep".to_string(),
        }];
        let r = apply_edits_to_normalized_content(content, &edits, "/x");
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().code, crate::error::FileErrorCode::NotFound);
    }

    #[test]
    fn apply_edits_duplicate_errors() {
        let content = "dup\nmid\ndup\n";
        let edits = vec![ReplaceEdit {
            old_text: "dup".to_string(),
            new_text: "x".to_string(),
        }];
        let r = apply_edits_to_normalized_content(content, &edits, "/x");
        assert!(r.is_err());
        let error = r.unwrap_err();
        assert_eq!(error.code, crate::error::FileErrorCode::Invalid);
        let message = error.to_string();
        assert!(message.contains("Match 1 at line 1"));
        assert!(message.contains("Match 2 at line 3"));
        assert!(message.contains("2: mid"));
    }

    #[test]
    fn duplicate_diagnostics_preserve_original_unicode_and_batch_index() {
        let edits = vec![
            ReplaceEdit {
                old_text: "heading".into(),
                new_text: "title".into(),
            },
            ReplaceEdit {
                old_text: "it's".into(),
                new_text: "changed".into(),
            },
        ];
        let error = apply_edits_to_normalized_content(
            "heading\n第一处 it’s\n中间\n第二处 it's\n",
            &edits,
            "/x",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 occurrences of edits[1]"));
        assert!(error.contains("Match 1 at line 2"));
        assert!(error.contains("Match 2 at line 4"));
        assert!(error.contains("2: 第一处 it’s"));
    }

    #[test]
    fn duplicate_diagnostics_bound_matches_and_long_unicode_lines() {
        let content = format!("{} repeated\n", "界".repeat(1000)).repeat(20);
        let context = duplicate_match_context(&content, "repeated");
        assert!(context.contains("Match 5 at line 5"));
        assert!(!context.contains("Match 6"));
        assert!(context.contains("More matches omitted"));
        assert!(context.contains("[line truncated]"));
        assert!(context.len() < 12_000);
    }

    #[test]
    fn apply_edits_overlap_errors() {
        let content = "overlap region here\n";
        let edits = vec![
            ReplaceEdit {
                old_text: "overlap region".to_string(),
                new_text: "A".to_string(),
            },
            ReplaceEdit {
                old_text: "region here".to_string(),
                new_text: "B".to_string(),
            },
        ];
        let r = apply_edits_to_normalized_content(content, &edits, "/x");
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("overlap"));
    }

    #[test]
    fn apply_edits_no_change_errors() {
        let content = "same\n";
        let edits = vec![ReplaceEdit {
            old_text: "same".to_string(),
            new_text: "same".to_string(),
        }];
        let r = apply_edits_to_normalized_content(content, &edits, "/x");
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("No changes"));
    }

    #[test]
    fn generate_unified_patch_has_headers() {
        let patch = generate_unified_patch("/x", "a\nb\n", "a\nB\n");
        assert!(patch.contains("--- /x"));
        assert!(patch.contains("+++ /x"));
        assert!(patch.contains("-b"));
        assert!(patch.contains("+B"));
    }
}
