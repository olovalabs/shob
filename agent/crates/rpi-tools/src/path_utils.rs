//! Mirrors `packages/agent/src/harness/tools/path-utils.ts` — tool-path
//! normalization + resolution, including the read tool's fuzzy-existence lookup
//! of typographic variants the model commonly emits.
//!
//! `resolve_tool_path` (write/edit) normalizes unicode spaces + strips a leading
//! `@`, then delegates to `env.absolute_path`. `resolve_read_tool_path` (read)
//! additionally tries NFKC-ish/typographic variants (smart quotes, narrow spaces
//! in AM/PM timestamps, NFD decomposition) and returns the first that actually
//! exists on disk — falling back to the original resolved path when none do.

use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::FileError;

/// Unicode-space characters replaced with a regular ASCII space by
/// [`normalize_tool_path`]. Same set as `normalizeForFuzzyMatch`'s space pass.
const UNICODE_SPACES: &[char] = &[
    '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}',
    '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
];

/// The narrow no-break space (U+202F) used in AM/PM timestamp variants.
const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// Replace unicode spaces with ASCII space and strip a leading `@`. Mirrors
/// `normalizeToolPath`.
pub fn normalize_tool_path(path: &str) -> String {
    let mut out: String = path
        .chars()
        .map(|c| if UNICODE_SPACES.contains(&c) { ' ' } else { c })
        .collect();
    if out.starts_with('@') {
        out.remove(0);
    }
    out
}

/// Resolve a tool path for write/edit: normalize, then `env.absolute_path`.
/// Mirrors `resolveToolPath`.
pub async fn resolve_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    cancel: Option<&CancellationToken>,
) -> Result<String, FileError> {
    let normalized = normalize_tool_path(path);
    let abs = env.absolute_path(&normalized, cancel).await?;
    Ok(abs.to_string_lossy().into_owned())
}

/// Resolve a read tool path: try the normalized-resolved path first, and if it
/// doesn't exist, try typographic variants (smart quotes, narrow spaces in
/// AM/PM timestamps, NFD decomposition) — returning the first that exists.
/// Falls back to the original resolved path when none exist. Mirrors
/// `resolveReadToolPath`.
pub async fn resolve_read_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    cancel: Option<&CancellationToken>,
) -> Result<String, FileError> {
    let resolved = resolve_tool_path(env, path, cancel).await?;

    // Build the candidate variant list in insertion order. Dedupe via a Vec
    // (preserve order; small N).
    let mut variants: Vec<String> = Vec::new();
    variants.push(resolved.clone());
    // AM/PM: regular-space AM/PM. → narrow-NBSP AM/PM. (case-insensitive, only
    // the space immediately before AM/PM).
    variants.push(replace_am_pm_space(&resolved));
    // NFD decomposition.
    variants.push(nfd(&resolved));
    // ASCII apostrophe → right single quote.
    variants.push(resolved.replace('\'', "\u{2019}"));
    // NFD + apostrophe.
    variants.push(replace_apostrophe(&nfd(&resolved), '\u{2019}'));

    // Dedupe preserving insertion order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let deduped: Vec<String> = variants
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect();

    for variant in &deduped {
        if env.exists(variant, cancel).await? {
            return Ok(variant.clone());
        }
    }
    // None exist → return the original resolved path (the read will then fail
    // with not_found, matching TS).
    Ok(resolved)
}

/// Replace ` AM.`/` PM.` (case-insensitive) with `<narrow-nbsp>AM.`/`<narrow-nbsp>PM.`.
/// Mirrors `resolved.replace(/ (AM|PM)\./gi, " $1.")`.
fn replace_am_pm_space(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for b" AM." / b" PM." (ASCII, case-insensitive) at a char boundary.
        if i + 3 < bytes.len() && bytes[i] == b' ' {
            let a = bytes[i + 1].to_ascii_lowercase();
            let m = bytes[i + 2].to_ascii_lowercase();
            let dot = bytes[i + 3];
            if (a == b'a' || a == b'p') && m == b'm' && dot == b'.' {
                // Emit narrow-NBSP + original-case AM/PM + '.'
                out.push(NARROW_NO_BREAK_SPACE);
                out.push(bytes[i + 1] as char);
                out.push(bytes[i + 2] as char);
                out.push('.');
                i += 4;
                continue;
            }
        }
        // Copy one UTF-8 char.
        let ch_len = utf8_char_len(bytes[i]);
        let end = std::cmp::min(i + ch_len, bytes.len());
        if let Ok(slice) = std::str::from_utf8(&bytes[i..end]) {
            out.push_str(slice);
        }
        i = end;
    }
    out
}

fn replace_apostrophe(s: &str, with: char) -> String {
    s.replace('\'', &with.to_string())
}

fn nfd(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd().collect()
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte >> 5 == 0b110 {
        2
    } else if first_byte >> 4 == 0b1110 {
        3
    } else if first_byte >> 3 == 0b11110 {
        4
    } else {
        // Invalid leading byte — consume 1 to make progress.
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_at_and_unicode_spaces() {
        assert_eq!(normalize_tool_path("@/foo/bar"), "/foo/bar");
        assert_eq!(normalize_tool_path("\u{00A0}foo"), " foo");
        assert_eq!(normalize_tool_path("a\u{2003}b"), "a b");
        // No leading @ → unchanged.
        assert_eq!(normalize_tool_path("foo/bar"), "foo/bar");
    }

    #[test]
    fn replace_am_pm_space_replaces_only_before_ampm_dot() {
        let s = "log 2024-01-01 2 PM.txt";
        let out = replace_am_pm_space(s);
        assert_eq!(out, "log 2024-01-01 2\u{202F}PM.txt");
        // AM case.
        let s2 = "log 10 AM.json";
        assert_eq!(replace_am_pm_space(s2), "log 10\u{202F}AM.json");
        // Non-AM/PM space preserved.
        let s3 = "a b c.txt";
        assert_eq!(replace_am_pm_space(s3), "a b c.txt");
    }

    #[test]
    fn nfd_decomposes() {
        // Precomposed é (U+00E9) → e + combining acute (U+0065 U+0301).
        let precomposed = "café";
        let decomposed = nfd(precomposed);
        assert_ne!(precomposed, decomposed);
        assert_eq!(decomposed.chars().count(), 5); // c a f e + combining
    }

    #[test]
    fn apostrophe_to_smart_quote() {
        let s = "it's a file";
        let out = replace_apostrophe(s, '\u{2019}');
        assert_eq!(out, "it\u{2019}s a file");
    }
}
