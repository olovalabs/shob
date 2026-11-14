//! Mirrors `packages/agent/src/harness/prompt-templates.ts` — markdown
//! template loading + argument substitution + invocation formatting.
//!
//! Loads `.md` files (non-recursively from directories; explicit files),
//! parses YAML frontmatter for an optional `description` (falling back to the
//! first non-blank body line, truncated at 60 chars + `...`), and substitutes
//! positional argument placeholders (`$N`, `$ARGUMENTS`, `$@`, `${@:N}`,
//! `${@:N:L}`) via [`substitute_args`].
//!
//! The TS `yaml` parse is replaced by the shared minimal YAML-subset parser
//! ([`crate::frontmatter::parse_frontmatter`]); see the initial port notes (retired).

use std::sync::Arc;

use rpi_tools::env::{ExecutionEnv, FileKind};
use tokio_util::sync::CancellationToken;

use crate::frontmatter::parse_frontmatter;
use crate::types::PromptTemplate;

/// Stable diagnostic codes. Mirrors TS `PromptTemplateDiagnosticCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTemplateDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
}

impl PromptTemplateDiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptTemplateDiagnosticCode::FileInfoFailed => "file_info_failed",
            PromptTemplateDiagnosticCode::ListFailed => "list_failed",
            PromptTemplateDiagnosticCode::ReadFailed => "read_failed",
            PromptTemplateDiagnosticCode::ParseFailed => "parse_failed",
        }
    }
}

/// Warning produced while loading prompt templates. Mirrors TS
/// `PromptTemplateDiagnostic`.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptTemplateDiagnostic {
    pub code: PromptTemplateDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// Result of loading prompt templates. Mirrors the TS `loadPromptTemplates`
/// return shape.
#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesResult {
    pub prompt_templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<PromptTemplateDiagnostic>,
    /// Structured winner/loser collision diagnostics (native
    /// `ResourceDiagnostic`).
    pub resource_diagnostics: Vec<crate::diagnostics::ResourceDiagnostic>,
}

/// Load prompt templates from one or more paths. Mirrors TS `loadPromptTemplates`.
///
/// Directory inputs load direct `.md` children non-recursively. File inputs
/// load explicit `.md` files. Missing paths (`not_found`) are skipped silently;
/// other `fileInfo`/list/read/parse failures become diagnostics.
pub async fn load_prompt_templates(
    env: &Arc<dyn ExecutionEnv>,
    paths: &[String],
) -> LoadPromptTemplatesResult {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    for path in paths {
        let cancel = CancellationToken::new();
        let info = match env.file_info(path, Some(&cancel)).await {
            Ok(i) => i,
            Err(e) => {
                if e.code != rpi_tools::FileErrorCode::NotFound {
                    diagnostics.push(PromptTemplateDiagnostic {
                        code: PromptTemplateDiagnosticCode::FileInfoFailed,
                        message: e.message,
                        path: path.clone(),
                    });
                }
                continue;
            }
        };
        let kind = resolve_kind(env, &info, &mut diagnostics, &cancel).await;
        if kind == Some(FileKind::Directory) {
            let mut result = load_templates_from_dir(env, &info.path.to_string_lossy()).await;
            prompt_templates.append(&mut result.prompt_templates);
            diagnostics.append(&mut result.diagnostics);
        } else if kind == Some(FileKind::File) && info.name.ends_with(".md") {
            let mut result =
                load_template_from_file(env, &info.path.to_string_lossy(), &info.name).await;
            if let Some(t) = result.template.take() {
                prompt_templates.push(t);
            }
            diagnostics.append(&mut result.diagnostics);
        }
    }
    LoadPromptTemplatesResult {
        prompt_templates,
        diagnostics,
        ..Default::default()
    }
}

/// Load prompt templates from source-tagged paths. Mirrors TS
/// `loadSourcedPromptTemplates`. `S` is the caller-owned provenance type.
pub async fn load_sourced_prompt_templates<S: Clone>(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[SourcedTemplateInput<S>],
) -> SourcedLoadTemplatesResult<S> {
    let mut templates: Vec<SourcedTemplate<S>> = Vec::new();
    let mut diagnostics: Vec<SourcedTemplateDiagnostic<S>> = Vec::new();
    for input in inputs {
        let result = load_prompt_templates(env, std::slice::from_ref(&input.path)).await;
        for t in result.prompt_templates {
            templates.push(SourcedTemplate {
                template: t,
                source: input.source.clone(),
            });
        }
        for d in result.diagnostics {
            diagnostics.push(SourcedTemplateDiagnostic {
                diagnostic: d,
                source: input.source.clone(),
            });
        }
    }
    SourcedLoadTemplatesResult {
        templates,
        diagnostics,
    }
}

/// One source-tagged input. Mirrors TS `{ path: string; source: TSource }`.
#[derive(Debug, Clone)]
pub struct SourcedTemplateInput<S> {
    pub path: String,
    pub source: S,
}

/// A template paired with its source. Mirrors TS `{ promptTemplate, source }`.
#[derive(Debug, Clone)]
pub struct SourcedTemplate<S> {
    pub template: PromptTemplate,
    pub source: S,
}

/// A diagnostic paired with its source.
#[derive(Debug, Clone)]
pub struct SourcedTemplateDiagnostic<S> {
    pub diagnostic: PromptTemplateDiagnostic,
    pub source: S,
}

/// Result of `load_sourced_prompt_templates`.
#[derive(Debug, Clone, Default)]
pub struct SourcedLoadTemplatesResult<S> {
    pub templates: Vec<SourcedTemplate<S>>,
    pub diagnostics: Vec<SourcedTemplateDiagnostic<S>>,
}

async fn load_templates_from_dir(
    env: &Arc<dyn ExecutionEnv>,
    dir: &str,
) -> LoadPromptTemplatesResult {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    let cancel = CancellationToken::new();
    let entries = match env.list_dir(dir, Some(&cancel)).await {
        Ok(e) => e,
        Err(e) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: PromptTemplateDiagnosticCode::ListFailed,
                message: e.message,
                path: dir.to_string(),
            });
            return LoadPromptTemplatesResult {
                prompt_templates,
                diagnostics,
                ..Default::default()
            };
        }
    };

    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in sorted {
        let kind = resolve_kind(env, entry, &mut diagnostics, &cancel).await;
        if kind != Some(FileKind::File) || !entry.name.ends_with(".md") {
            continue;
        }
        let mut result =
            load_template_from_file(env, &entry.path.to_string_lossy(), &entry.name).await;
        if let Some(t) = result.template.take() {
            prompt_templates.push(t);
        }
        diagnostics.append(&mut result.diagnostics);
    }
    LoadPromptTemplatesResult {
        prompt_templates,
        diagnostics,
        ..Default::default()
    }
}

struct LoadTemplateResult {
    template: Option<PromptTemplate>,
    diagnostics: Vec<PromptTemplateDiagnostic>,
}

async fn load_template_from_file(
    env: &Arc<dyn ExecutionEnv>,
    file_path: &str,
    file_name: &str,
) -> LoadTemplateResult {
    let mut diagnostics = Vec::new();
    let cancel = CancellationToken::new();
    let raw = match env.read_text_file(file_path, Some(&cancel)).await {
        Ok(s) => s,
        Err(e) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: PromptTemplateDiagnosticCode::ReadFailed,
                message: e.message,
                path: file_path.to_string(),
            });
            return LoadTemplateResult {
                template: None,
                diagnostics,
            };
        }
    };

    let (frontmatter, body) = match parse_frontmatter(&raw) {
        Ok(v) => v,
        Err(msg) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: PromptTemplateDiagnosticCode::ParseFailed,
                message: msg,
                path: file_path.to_string(),
            });
            return LoadTemplateResult {
                template: None,
                diagnostics,
            };
        }
    };

    // First non-blank body line (for description fallback).
    let first_line = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.to_string());
    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            first_line.as_ref().map(|fl| {
                if fl.chars().count() > 60 {
                    let mut s: String = fl.chars().take(60).collect();
                    s.push_str("...");
                    s
                } else {
                    fl.clone()
                }
            })
        });

    LoadTemplateResult {
        template: Some(PromptTemplate {
            // Mirrors TS `fileName.replace(/\.md$/i, "")` — a single
            // case-insensitive trailing `.md` strip. `ends_with(".md")` (case-
            // sensitive) gates entry above, so the case-insensitivity is moot in
            // practice; `strip_suffix` strips at most one occurrence (the TS
            // single-replace, NOT `trim_end_matches`'s repeated strip).
            name: file_name
                .strip_suffix(".md")
                .or_else(|| file_name.strip_suffix(".MD"))
                .unwrap_or(file_name)
                .to_string(),
            description,
            content: body,
        }),
        diagnostics,
    }
}

async fn resolve_kind(
    env: &Arc<dyn ExecutionEnv>,
    info: &rpi_tools::env::FileInfo,
    diagnostics: &mut Vec<PromptTemplateDiagnostic>,
    cancel: &CancellationToken,
) -> Option<FileKind> {
    if matches!(info.kind, FileKind::File | FileKind::Directory) {
        return Some(info.kind);
    }
    match env
        .canonical_path(&info.path.to_string_lossy(), Some(cancel))
        .await
    {
        Ok(canon) => match env.file_info(&canon.to_string_lossy(), Some(cancel)).await {
            Ok(target) => {
                if matches!(target.kind, FileKind::File | FileKind::Directory) {
                    Some(target.kind)
                } else {
                    None
                }
            }
            Err(e) => {
                if e.code != rpi_tools::FileErrorCode::NotFound {
                    diagnostics.push(PromptTemplateDiagnostic {
                        code: PromptTemplateDiagnosticCode::FileInfoFailed,
                        message: e.message,
                        path: info.path.to_string_lossy().to_string(),
                    });
                }
                None
            }
        },
        Err(e) => {
            if e.code != rpi_tools::FileErrorCode::NotFound {
                diagnostics.push(PromptTemplateDiagnostic {
                    code: PromptTemplateDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: info.path.to_string_lossy().to_string(),
                });
            }
            None
        }
    }
}

/// Parse an argument string using simple shell-style single and double quotes.
/// Mirrors TS `parseCommandArgs`.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for ch in args_string.chars() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute prompt-template placeholders (`$1`, `$@`, `$ARGUMENTS`,
/// `${@:N}`, `${@:N:L}`) with command arguments. Mirrors TS `substituteArgs`.
///
/// Order of replacement (matches TS exactly so a `$1` inside an expanded
/// `$ARGUMENTS` is NOT re-expanded): `$N` → `${@:N}`/`${@:N:L}` → `$ARGUMENTS`
/// → `$@`.
///
/// - `$N` → `args[N-1]` (empty if out of range).
/// - `${@:N}` → `args[N-1..].join(" ")` (1-indexed; clamped to 0).
/// - `${@:N:L}` → `args[N-1..N-1+L].join(" ")`.
/// - `$ARGUMENTS` / `$@` → `args.join(" ")`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let result = replace_dollar_n(content, args);
    let result = replace_at_colon(&result, args);
    let all_args = args.join(" ");
    let result = result.replace("$ARGUMENTS", &all_args);
    result.replace("$@", &all_args)
}

/// Format a prompt-template invocation with positional arguments. Mirrors TS
/// `formatPromptTemplateInvocation`.
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}

// Replace `$(\d+)` with `args[n-1]` (empty if out of range). Mirrors the TS
// `result.replace(/\$(\d+)/g, …)` regex. Hand-rolled to avoid a regex dep.
fn replace_dollar_n(content: &str, args: &[String]) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // Consume the maximal run of digits.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let digits = std::str::from_utf8(&bytes[i + 1..j]).unwrap_or("");
            let n: usize = digits.parse().unwrap_or(0);
            if n >= 1 {
                if let Some(arg) = args.get(n - 1) {
                    out.push_str(arg);
                }
            }
            // n==0 (digits "0") → TS `args[parseInt("0")-1] ?? ""` = args[-1] →
            // undefined → "". Matches: push nothing.
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// Replace `${@:N}` and `${@:N:L}`. Mirrors the TS regex
// `\$\{@:(\d+)(?::(\d+))?\}`.
fn replace_at_colon(content: &str, args: &[String]) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for `${@:` at this position.
        if i + 3 < bytes.len()
            && bytes[i] == b'$'
            && bytes[i + 1] == b'{'
            && bytes[i + 2] == b'@'
            && bytes[i + 3] == b':'
        {
            // Consume digits for N.
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 4 {
                let n_str = std::str::from_utf8(&bytes[i + 4..j]).unwrap_or("");
                let mut start: usize = n_str.parse::<usize>().unwrap_or(0).saturating_sub(1);
                // TS: `if (start < 0) start = 0;` — usize clamp mirrors it.
                if j < bytes.len() && bytes[j] == b':' {
                    // `${@:N:L}` form.
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b'}' && k > j + 1 {
                        let l_str = std::str::from_utf8(&bytes[j + 1..k]).unwrap_or("");
                        let length: usize = l_str.parse().unwrap_or(0);
                        let end = (start + length).min(args.len());
                        out.push_str(&args[start.min(args.len())..end].join(" "));
                        i = k + 1;
                        continue;
                    }
                } else if j < bytes.len() && bytes[j] == b'}' {
                    // `${@:N}` form.
                    let _ = &mut start;
                    let slice = if start >= args.len() {
                        &[][..]
                    } else {
                        &args[start..]
                    };
                    out.push_str(&slice.join(" "));
                    i = j + 1;
                    continue;
                }
            }
            // Not a recognized `${@:…}` form — emit `$` literally and continue
            // past it (the `{...}` will be copied verbatim by subsequent iters).
            out.push('$');
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_args_shell_style() {
        assert_eq!(parse_command_args("a b c"), vec!["a", "b", "c"]);
        assert_eq!(
            parse_command_args("'hello world' test"),
            vec!["hello world", "test"]
        );
        // Double quotes: the TS parser does NOT interpret backslash escapes, so
        // a `"` always toggles the quote state. `"a b"` -> ["a b"].
        assert_eq!(parse_command_args("\"a b\" c"), vec!["a b", "c"]);
        // A quote that opens then closes then re-opens collapses the gaps:
        // `"a"b"c"` -> a, b, c joined with no spaces => ["abc"].
        assert_eq!(parse_command_args("\"a\"b\"c\""), vec!["abc"]);
        assert_eq!(
            parse_command_args("  multi   space  "),
            vec!["multi", "space"]
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
    }

    #[test]
    fn substitute_args_all_forms() {
        let args = vec!["hello".to_string(), "world".to_string(), "test".to_string()];
        assert_eq!(substitute_args("$1", &args), "hello");
        assert_eq!(substitute_args("$2 $3", &args), "world test");
        assert_eq!(substitute_args("${@:2}", &args), "world test");
        assert_eq!(substitute_args("${@:1:2}", &args), "hello world");
        assert_eq!(substitute_args("$ARGUMENTS", &args), "hello world test");
        assert_eq!(substitute_args("$@", &args), "hello world test");
        // Out-of-range $N → empty.
        assert_eq!(substitute_args("$9", &args), "");
        // $0 → empty (TS: args[-1] ?? "").
        assert_eq!(substitute_args("$0", &args), "");
    }

    #[test]
    fn substitute_args_does_not_re_expand() {
        // `$ARGUMENTS` expansion should not be re-scanned for `$N`. Here the
        // args contain a literal `$1` that must survive intact.
        let args = vec!["$1".to_string()];
        assert_eq!(substitute_args("$ARGUMENTS", &args), "$1");
    }

    #[test]
    fn substitute_args_empty_args_join() {
        let args: Vec<String> = Vec::new();
        assert_eq!(substitute_args("$1 $@", &args), " ");
        // `$1` → "" ; `$@` → "" ; joined by the literal space in the template.
    }

    #[test]
    fn format_invocation_substitutes() {
        let t = PromptTemplate {
            name: "one".to_string(),
            description: None,
            content: "$1 ${@:2} $ARGUMENTS".to_string(),
        };
        let out =
            format_prompt_template_invocation(&t, &["hello world".to_string(), "test".to_string()]);
        assert_eq!(out, "hello world test hello world test");
    }

    #[test]
    fn template_name_strips_md_extension() {
        // Mirrors TS `fileName.replace(/\.md$/i, "")` — single (not repeated)
        // case-insensitive trailing `.md` strip.
        assert_eq!(strip_md("foo.md"), "foo");
        assert_eq!(strip_md("foo.MD"), "foo");
        assert_eq!(strip_md("foo"), "foo");
        assert_eq!(strip_md("foo.md.md"), "foo.md"); // single replace, not trim_end
    }

    fn strip_md(name: &str) -> &str {
        name.strip_suffix(".md")
            .or_else(|| name.strip_suffix(".MD"))
            .unwrap_or(name)
    }
}
