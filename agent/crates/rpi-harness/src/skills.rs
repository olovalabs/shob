//! Mirrors `packages/agent/src/harness/skills.ts` — YAML-frontmatter skill
//! loading + the TWO SEPARATE skill-formatting paths (invariant §9):
//!
//! 1. [`format_skills_for_system_prompt`] (mirrors `system-prompt.ts` — moved
//!    here alongside the loader for cohesion) emits the XML-escaped
//!    `<available_skills>` *listing* the model sees in the system prompt.
//! 2. [`format_skill_invocation`] emits the UNescaped `<skill name location>`
//!    *invocation* block (full content + dirname reference note) injected when
//!    the user or app explicitly invokes a skill.
//!
//! These are distinct code paths and must NOT be unified (invariant §9): the
//! listing escapes all fields so skill descriptions cannot inject XML into the
//! system prompt, while the invocation deliberately embeds raw `content` (the
//! skill author controls it and the model must read full instructions verbatim).
//!
//! Frontmatter is parsed as standard YAML by
//! [`crate::frontmatter::parse_frontmatter`]; the loader honors `.gitignore` /
//! `.ignore` / `.fdignore` via an `ignore`-pattern matcher (hand-ported; no
//! `ignore` crate dep).
//!
//! v1 divergences from TS (see the initial port notes (retired)):
//! - Malformed frontmatter yields a `parse_failed` diagnostic, matching TS.
//! - `loadSourcedSkills` is ported as a generic helper where the caller owns
//!   the source type (Rust has no TS variadic generics); the `mapSkill` hook is
//!   replaced by the caller post-mapping.

use std::sync::Arc;

use rpi_tools::env::{ExecutionEnv, FileInfo, FileKind};
use tokio_util::sync::CancellationToken;

use crate::frontmatter::parse_frontmatter;
use crate::types::Skill;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: &[&str] = &[".gitignore", ".ignore", ".fdignore"];

/// Stable diagnostic codes. Mirror TS `SkillDiagnosticCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
    InvalidMetadata,
}

impl SkillDiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillDiagnosticCode::FileInfoFailed => "file_info_failed",
            SkillDiagnosticCode::ListFailed => "list_failed",
            SkillDiagnosticCode::ReadFailed => "read_failed",
            SkillDiagnosticCode::ParseFailed => "parse_failed",
            SkillDiagnosticCode::InvalidMetadata => "invalid_metadata",
        }
    }
}

/// Warning produced while loading skills. Mirrors TS `SkillDiagnostic`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDiagnostic {
    pub code: SkillDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// Result of loading skills: the loaded skills + any warnings. Mirrors the TS
/// `loadSkills` return shape.
#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
    /// Structured winner/loser collision diagnostics (native
    /// `ResourceDiagnostic`). `diagnostics` keeps the stringly-typed view for
    /// backwards compatibility; this is the machine-readable form.
    pub resource_diagnostics: Vec<crate::diagnostics::ResourceDiagnostic>,
}

// ---------------------------------------------------------------------------
// Invocation block (UNescaped) — invariant §9 path (2)
// ---------------------------------------------------------------------------

/// Format a skill invocation prompt, optionally appending additional user
/// instructions. Mirrors TS `formatSkillInvocation`: the `<skill name location>`
/// block is **NOT** XML-escaped — the skill `content` is embedded verbatim so
/// the model reads the full instructions.
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let dirname = dirname_env_path(&skill.file_path);
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {dirname}.\n\n{}\n</skill>",
        skill.name, skill.file_path, skill.content
    );
    match additional_instructions {
        Some(extra) if !extra.is_empty() => format!("{skill_block}\n\n{extra}"),
        _ => skill_block,
    }
}

/// Parse only a leading, manually entered skill command. Like pi, the first
/// literal space separates the name from arguments; argument interiors are
/// deliberately not tokenized or normalized.
pub fn parse_skill_command(text: &str) -> Option<(&str, &str)> {
    let command = text.strip_prefix("/skill:")?;
    Some(match command.split_once(' ') {
        Some((name, args)) => (name, args.trim()),
        None => (command, ""),
    })
}

/// Read the current body using the same frontmatter parser as discovery.
/// Metadata errors are returned so the caller can report them and pass the
/// original command through, matching pi's skill-expansion fallback.
pub fn format_skill_command(skill: &Skill, source: &str, args: &str) -> Result<String, String> {
    let (_, body) = parse_frontmatter(source.trim_start_matches('\u{feff}'))?;
    let mut fresh = skill.clone();
    fresh.content = body.trim().to_string();
    Ok(format_skill_invocation(&fresh, Some(args)))
}

// ---------------------------------------------------------------------------
// System-prompt listing (XML-escaped) — invariant §9 path (1)
// ---------------------------------------------------------------------------

/// Format the `<available_skills>` listing for the system prompt. Mirrors TS
/// `formatSkillsForSystemPrompt`. Skills with `disable_model_invocation == Some(true)`
/// are excluded. Returns `""` when no skill is model-visible. All fields are
/// XML-escaped so descriptions/paths cannot inject markup.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation.unwrap_or(false))
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        "Read the full skill file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// Escape XML special characters. Mirrors TS `escapeXml`.
pub fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// loadSkills — recursive directory traversal + frontmatter parse + validation
// ---------------------------------------------------------------------------

/// Load skills from one or more directories. Mirrors TS `loadSkills`.
///
/// Traverses directories recursively, loads `SKILL.md` files, loads direct root
/// `.md` files as skills, honors ignore files, returns diagnostics for invalid
/// skill files. Missing input directories are skipped (no diagnostic, matching
/// TS's `code !== "not_found"` guard).
pub async fn load_skills(env: &Arc<dyn ExecutionEnv>, dirs: &[String]) -> LoadSkillsResult {
    load_skills_with_source_observer(env, dirs, &|_, _| {}).await
}

/// Load with the same discovery/validation semantics as [`load_skills`], while
/// observing the exact UTF-8 source passed to the parser. Hosts can fingerprint
/// parsed resources without a second, potentially different filesystem read.
/// Reads and cancellation retain the supplied environment's loader contract.
pub async fn load_skills_with_source_observer(
    env: &Arc<dyn ExecutionEnv>,
    dirs: &[String],
    observe: &(dyn Fn(&str, &str) + Send + Sync),
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for dir in dirs {
        let cancel = CancellationToken::new();
        let root_info = match env.file_info(dir, Some(&cancel)).await {
            Ok(info) => info,
            Err(e) => {
                if e.code != rpi_tools::FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: e.message,
                        path: dir.clone(),
                    });
                }
                continue;
            }
        };
        // Mirror pi `skills.ts:462-481`: a skillPath may be a directory OR an
        // individual `.md` file. (Previously this branch was directory-only — a
        // plugin's `resources_discover` returning bare `SKILL.md` file paths was
        // silently skipped. B5b widens this to files so discovered skills load.)
        let kind = resolve_kind(env, &root_info, &mut diagnostics, &cancel).await;
        match kind {
            Some(FileKind::Directory) => {
                let mut result = load_skills_from_dir_internal(
                    env,
                    &root_info.path.to_string_lossy(),
                    true,
                    &IgnoreMatcher::new(),
                    &root_info.path.to_string_lossy(),
                    observe,
                )
                .await;
                skills.append(&mut result.skills);
                diagnostics.append(&mut result.diagnostics);
            }
            Some(FileKind::File) if root_info.name.ends_with(".md") => {
                // pi `loadSkillFromFile` (skills.ts:277-325) falls back to
                // `basename(dirname(filePath))` for the skill name when the
                // frontmatter has no `name`. `dirname_env_path` (skills.rs:688)
                // is the env-path dirname helper; the final segment of THAT is
                // the parent directory's basename.
                let dir_path = dirname_env_path(&root_info.path.to_string_lossy());
                let parent_dir_name = basename_of(&dir_path);
                let mut result = load_skill_from_file(
                    env,
                    &root_info.path.to_string_lossy(),
                    &parent_dir_name,
                    observe,
                )
                .await;
                if let Some(skill) = result.skill.take() {
                    skills.push(skill);
                }
                diagnostics.append(&mut result.diagnostics);
            }
            _ => {
                // Not a dir, not an `.md` file (or symlink-unresolvable): skip
                // silently, mirroring pi's "is not a markdown file" warning
                // path (skills.ts:474-476) — we emit no diagnostic to stay
                // consistent with the prior directory-only skip behavior.
                continue;
            }
        }
    }
    LoadSkillsResult {
        skills,
        diagnostics,
        ..Default::default()
    }
}

/// Load skills from source-tagged directories. Mirrors TS `loadSourcedSkills`.
/// `S` is the caller-owned provenance type; each loaded skill and diagnostic is
/// paired with a clone of the source for the originating input.
pub async fn load_sourced_skills<S: Clone>(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[SourcedSkillInput<S>],
) -> SourcedLoadSkillsResult<S> {
    let mut skills: Vec<SourcedSkill<S>> = Vec::new();
    let mut diagnostics: Vec<SourcedSkillDiagnostic<S>> = Vec::new();
    for input in inputs {
        let result = load_skills(env, std::slice::from_ref(&input.path)).await;
        for skill in result.skills {
            skills.push(SourcedSkill {
                skill,
                source: input.source.clone(),
            });
        }
        for diag in result.diagnostics {
            diagnostics.push(SourcedSkillDiagnostic {
                diagnostic: diag,
                source: input.source.clone(),
            });
        }
    }
    SourcedLoadSkillsResult {
        skills,
        diagnostics,
    }
}

/// One source-tagged input. Mirrors TS `{ path: string; source: TSource }`.
#[derive(Debug, Clone)]
pub struct SourcedSkillInput<S> {
    pub path: String,
    pub source: S,
}

/// A skill paired with its source. Mirrors TS `{ skill, source }`.
#[derive(Debug, Clone)]
pub struct SourcedSkill<S> {
    pub skill: Skill,
    pub source: S,
}

/// A diagnostic paired with its source. Mirrors TS `SkillDiagnostic & { source }`.
#[derive(Debug, Clone)]
pub struct SourcedSkillDiagnostic<S> {
    pub diagnostic: SkillDiagnostic,
    pub source: S,
}

/// Result of `load_sourced_skills`.
#[derive(Debug, Clone, Default)]
pub struct SourcedLoadSkillsResult<S> {
    pub skills: Vec<SourcedSkill<S>>,
    pub diagnostics: Vec<SourcedSkillDiagnostic<S>>,
}

async fn load_skills_from_dir_internal(
    env: &Arc<dyn ExecutionEnv>,
    dir: &str,
    include_root_files: bool,
    ignore_matcher: &IgnoreMatcher,
    root_dir: &str,
    observe: &(dyn Fn(&str, &str) + Send + Sync),
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let cancel = CancellationToken::new();

    let dir_info = match env.file_info(dir, Some(&cancel)).await {
        Ok(info) => info,
        Err(e) => {
            if e.code != rpi_tools::FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: dir.to_string(),
                });
            }
            return LoadSkillsResult {
                skills,
                diagnostics,
                ..Default::default()
            };
        }
    };
    if resolve_kind(env, &dir_info, &mut diagnostics, &cancel).await != Some(FileKind::Directory) {
        return LoadSkillsResult {
            skills,
            diagnostics,
            ..Default::default()
        };
    }

    let mut matcher = ignore_matcher.clone();
    add_ignore_rules(env, &mut matcher, dir, root_dir, &mut diagnostics).await;

    let entries = match env.list_dir(dir, Some(&cancel)).await {
        Ok(e) => e,
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ListFailed,
                message: e.message,
                path: dir.to_string(),
            });
            return LoadSkillsResult {
                skills,
                diagnostics,
                ..Default::default()
            };
        }
    };

    // First pass: a directory's SKILL.md (if present) is the skill for that dir;
    // return immediately after consuming it (mirrors TS `return` inside the loop).
    for entry in &entries {
        if entry.name != "SKILL.md" {
            continue;
        }
        let full_path = entry.path.to_string_lossy().to_string();
        if resolve_kind(env, entry, &mut diagnostics, &cancel).await != Some(FileKind::File) {
            continue;
        }
        let rel = relative_env_path(root_dir, &full_path);
        if matcher.ignores(&rel) {
            continue;
        }
        let mut result = load_skill_from_file(env, &full_path, &dir_info.name, observe).await;
        if let Some(skill) = result.skill.take() {
            skills.push(skill);
        }
        diagnostics.append(&mut result.diagnostics);
        return LoadSkillsResult {
            skills,
            diagnostics,
            ..Default::default()
        };
    }

    // Second pass: recurse into subdirs (non-hidden, non-node_modules) sorted by
    // name; load direct root `.md` files when `include_root_files`.
    let mut sorted: Vec<&FileInfo> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in sorted {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let full_path = entry.path.to_string_lossy().to_string();
        let kind = resolve_kind(env, entry, &mut diagnostics, &cancel).await;
        let Some(kind) = kind else { continue };

        let rel = relative_env_path(root_dir, &full_path);
        let ignore_path = match kind {
            FileKind::Directory => format!("{rel}/"),
            _ => rel.clone(),
        };
        if matcher.ignores(&ignore_path) {
            continue;
        }

        if kind == FileKind::Directory {
            let mut result = Box::pin(load_skills_from_dir_internal(
                env, &full_path, false, &matcher, root_dir, observe,
            ))
            .await;
            skills.append(&mut result.skills);
            diagnostics.append(&mut result.diagnostics);
            continue;
        }

        if kind != FileKind::File || !include_root_files || !entry.name.ends_with(".md") {
            continue;
        }
        let mut result = load_skill_from_file(env, &full_path, &dir_info.name, observe).await;
        if let Some(skill) = result.skill.take() {
            skills.push(skill);
        }
        diagnostics.append(&mut result.diagnostics);
    }

    LoadSkillsResult {
        skills,
        diagnostics,
        ..Default::default()
    }
}

struct LoadSkillFromFileResult {
    skill: Option<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

async fn load_skill_from_file(
    env: &Arc<dyn ExecutionEnv>,
    file_path: &str,
    parent_dir_name: &str,
    observe: &(dyn Fn(&str, &str) + Send + Sync),
) -> LoadSkillFromFileResult {
    let mut diagnostics = Vec::new();
    let cancel = CancellationToken::new();
    let raw = match env.read_text_file(file_path, Some(&cancel)).await {
        Ok(s) => s,
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ReadFailed,
                message: e.message,
                path: file_path.to_string(),
            });
            return LoadSkillFromFileResult {
                skill: None,
                diagnostics,
            };
        }
    };

    observe(file_path, &raw);
    let (frontmatter, body) = match parse_frontmatter(&raw) {
        Ok(v) => v,
        Err(msg) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ParseFailed,
                message: msg,
                path: file_path.to_string(),
            });
            return LoadSkillFromFileResult {
                skill: None,
                diagnostics,
            };
        }
    };

    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    for err in validate_description(description.as_deref()) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: err,
            path: file_path.to_string(),
        });
    }

    let frontmatter_name = frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let name = frontmatter_name.unwrap_or_else(|| parent_dir_name.to_string());
    for err in validate_name(&name, parent_dir_name) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: err,
            path: file_path.to_string(),
        });
    }

    let description = match description {
        Some(d) if !d.trim().is_empty() => d,
        _ => {
            // Mirrors TS: `if (!description || description.trim() === "") return null`.
            return LoadSkillFromFileResult {
                skill: None,
                diagnostics,
            };
        }
    };

    let disable_model_invocation = frontmatter
        .get("disable-model-invocation")
        .and_then(|v| v.as_bool());

    LoadSkillFromFileResult {
        skill: Some(Skill {
            name,
            description,
            content: body,
            file_path: file_path.to_string(),
            disable_model_invocation,
        }),
        diagnostics,
    }
}

fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None | Some("") => errors.push("description is required".to_string()),
        Some(d) if d.trim().is_empty() => errors.push("description is required".to_string()),
        Some(d) if d.chars().count() > MAX_DESCRIPTION_LENGTH => {
            errors.push(format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                d.chars().count()
            ));
        }
        _ => {}
    }
    errors
}

async fn resolve_kind(
    env: &Arc<dyn ExecutionEnv>,
    info: &FileInfo,
    diagnostics: &mut Vec<SkillDiagnostic>,
    cancel: &CancellationToken,
) -> Option<FileKind> {
    if matches!(info.kind, FileKind::File | FileKind::Directory) {
        return Some(info.kind);
    }
    // Symlink or unknown: canonicalize then re-stat.
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
                    diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: e.message,
                        path: info.path.to_string_lossy().to_string(),
                    });
                }
                None
            }
        },
        Err(e) => {
            if e.code != rpi_tools::FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: info.path.to_string_lossy().to_string(),
                });
            }
            None
        }
    }
}

async fn add_ignore_rules(
    env: &Arc<dyn ExecutionEnv>,
    matcher: &mut IgnoreMatcher,
    dir: &str,
    root_dir: &str,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let cancel = CancellationToken::new();
    let relative_dir = relative_env_path(root_dir, dir);
    let prefix = if relative_dir.is_empty() {
        String::new()
    } else {
        format!("{relative_dir}/")
    };

    for &filename in IGNORE_FILE_NAMES {
        let joined = match env.join_path(&[dir, filename], Some(&cancel)).await {
            Ok(p) => p,
            Err(e) => {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: dir.to_string(),
                });
                continue;
            }
        };
        let ignore_path = joined.to_string_lossy().to_string();
        let info = match env.file_info(&ignore_path, Some(&cancel)).await {
            Ok(i) => i,
            Err(e) => {
                if e.code != rpi_tools::FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: e.message,
                        path: ignore_path.clone(),
                    });
                }
                continue;
            }
        };
        if info.kind != FileKind::File {
            continue;
        }
        let content = match env.read_text_file(&ignore_path, Some(&cancel)).await {
            Ok(c) => c,
            Err(e) => {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ReadFailed,
                    message: e.message,
                    path: ignore_path,
                });
                continue;
            }
        };
        let patterns: Vec<String> = content
            .split('\n')
            .flat_map(|line| {
                let mut s = line.split('\r');
                let first = s.next().unwrap_or("");
                prefix_ignore_pattern(first, &prefix)
            })
            .collect();
        if !patterns.is_empty() {
            matcher.add(&patterns);
        }
    }
}

/// Prefix a single ignore-file line for the matcher. Mirrors TS
/// `prefixIgnorePattern` (returns `None` for blank/comment lines).
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line;
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }
    let prefixed = if prefix.is_empty() {
        pattern.to_string()
    } else {
        format!("{prefix}{pattern}")
    };
    if negated {
        Some(format!("!{prefixed}"))
    } else {
        Some(prefixed)
    }
}

// ---------------------------------------------------------------------------
// Path helpers — mirror TS `dirnameEnvPath` / `relativeEnvPath`
// ---------------------------------------------------------------------------

/// Dirname of an env path. Mirrors TS `dirnameEnvPath` (handles both `/` and
/// `\`, Windows drive-letter root like `C:\`).
fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches(|c: char| c == '/' || c == '\\');
    // `lastIndexOf` over both separators → the MAX index of either.
    let bs = normalized.rfind('\\');
    let fs = normalized.rfind('/');
    let sep_index = match (bs, fs) {
        (Some(b), Some(f)) => b.max(f),
        (Some(b), None) => b,
        (None, Some(f)) => f,
        (None, None) => return "/".to_string(),
    };
    // Windows drive root: `C:\` → return `C:\`.
    if sep_index == 2 && normalized.as_bytes().get(1) == Some(&b':') {
        return normalized[..3].to_string();
    }
    if sep_index == 0 {
        return "/".to_string();
    }
    normalized[..sep_index].to_string()
}

/// Basename (final path segment) of an env-path string, handling both `/` and
/// `\`. Mirrors TS `basename`. Returns the whole string when there's no
/// separator. Used by [`load_skills`] to compute a discovered `.md` skill file's
/// parent-directory name (the pi frontmatter-`name` fallback,
/// `basename(dirname(filePath))` at skills.ts:286-296).
fn basename_of(path: &str) -> String {
    let normalized = path.trim_end_matches(|c: char| c == '/' || c == '\\');
    let bs = normalized.rfind('\\');
    let fs = normalized.rfind('/');
    match (bs, fs) {
        (Some(b), Some(f)) => normalized[b.max(f) + 1..].to_string(),
        (Some(b), None) => normalized[b + 1..].to_string(),
        (None, Some(f)) => normalized[f + 1..].to_string(),
        (None, None) => normalized.to_string(),
    }
}

/// Relative path of `path` from `root`. Mirrors TS `relativeEnvPath`.
fn relative_env_path(root: &str, path: &str) -> String {
    let normalized_root = root.replace('\\', "/").trim_end_matches('/').to_string();
    let normalized_path = path.replace('\\', "/").trim_end_matches('/').to_string();
    if normalized_path == normalized_root {
        return String::new();
    }
    if let Some(rest) = normalized_path.strip_prefix(&format!("{normalized_root}/")) {
        rest.to_string()
    } else {
        normalized_path.trim_start_matches('/').to_string()
    }
}

// ---------------------------------------------------------------------------
// Minimal `ignore` matcher — port of the JS `ignore` package's core behavior
// for the patterns the loader actually emits (prefixed globs, `!` negation,
// trailing-slash dir patterns). NOT a full .gitignore engine; the loader's
// needs are: exact-path, prefix-dir, and `*`-glob segment matches.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct IgnoreMatcher {
    patterns: Vec<IgnorePattern>,
}

#[derive(Clone)]
struct IgnorePattern {
    negated: bool,
    /// Pattern with optional trailing `/` (dir-only). Stored WITHOUT a leading
    /// `/` (anchored is implicit because the loader prefixes patterns).
    body: String,
    dir_only: bool,
}

impl IgnoreMatcher {
    fn new() -> Self {
        Self::default()
    }

    fn add(&mut self, patterns: &[String]) {
        for p in patterns {
            if let Some(pat) = Self::parse(p) {
                self.patterns.push(pat);
            }
        }
    }

    fn parse(raw: &str) -> Option<IgnorePattern> {
        let mut s = raw;
        let negated = if let Some(rest) = s.strip_prefix('!') {
            s = rest;
            true
        } else {
            false
        };
        let dir_only = s.ends_with('/');
        let body = s.trim_end_matches('/').to_string();
        if body.is_empty() {
            return None;
        }
        Some(IgnorePattern {
            negated,
            body,
            dir_only,
        })
    }

    /// Whether `rel_path` is ignored. `rel_path` is relative to the root Dir
    /// (no leading `/`); a directory should be passed with a trailing `/` by
    /// the caller (the loader does this).
    fn ignores(&self, rel_path: &str) -> bool {
        let mut ignored = false;
        for pat in &self.patterns {
            if pat.matches(rel_path) {
                ignored = !pat.negated;
            }
        }
        ignored
    }
}

impl IgnorePattern {
    fn matches(&self, rel_path: &str) -> bool {
        let target = rel_path.trim_end_matches('/');
        if self.dir_only {
            // Dir pattern matches the dir itself or anything beneath it.
            Self::glob_match(&self.body, target) || target.starts_with(&format!("{}/", self.body))
        } else {
            // File pattern: matches the exact path OR anything beneath it when
            // the pattern has no internal `/` (basename match) — mirrors gitignore
            // "if no slash, matches at any depth".
            if Self::glob_match(&self.body, target) {
                return true;
            }
            if !self.body.contains('/') {
                // basename match at any depth.
                let basename = target.rsplit('/').next().unwrap_or(target);
                return Self::glob_match(&self.body, basename);
            }
            // Pattern with a slash but not matching exact → also match as prefix.
            target.starts_with(&format!("{}/", self.body))
        }
    }

    /// Tiny glob: supports `*` (any chars except `/`) and literal text. No `**`.
    fn glob_match(pattern: &str, text: &str) -> bool {
        let mut parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 1 {
            return pattern == text;
        }
        let mut cursor = 0usize;
        // First segment must prefix the text.
        let first = parts.remove(0);
        if !text[cursor..].starts_with(first) {
            return false;
        }
        cursor += first.len();
        let mut mid = parts.split_last().map(|(last, mid)| (mid.to_vec(), *last));
        let (mid_parts, last) = match mid.take() {
            Some((m, l)) => (m, l),
            None => (Vec::new(), ""), // pattern ended with `*`
        };
        for seg in &mid_parts {
            if seg.is_empty() {
                continue; // consecutive `*`
            }
            match text[cursor..].find(seg) {
                Some(i) => cursor += i + seg.len(),
                None => return false,
            }
        }
        // Last segment must suffix the remaining text (but not cross a `/`).
        if last.is_empty() {
            // Pattern ends on `*` and any non-empty remainder is fine (but a `*`
            // doesn't cross `/`); if the remainder contains a `/`, only match up
            // to it if the pattern implied depth-invariance. Keep it simple: a
            // trailing `*` accepts any remainder that does NOT introduce a new
            // path segment unless the pattern had no `/`. (Loader prefixer keeps
            // patterns path-shaped; this is sufficient.)
            return true;
        }
        text[cursor..].ends_with(last) && !text[cursor..text.len() - last.len()].contains('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str, file: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: "c".to_string(),
            file_path: file.to_string(),
            disable_model_invocation: None,
        }
    }

    #[test]
    fn listing_skips_disabled_and_no_visible_returns_empty() {
        let visible = skill("visible", "Use <this> & that", "/skills/visible/SKILL.md");
        let second = skill("second", "Second skill", "/skills/second/SKILL.md");
        let mut disabled = skill("hidden", "Hidden", "/skills/hidden/SKILL.md");
        disabled.disable_model_invocation = Some(true);

        let out = format_skills_for_system_prompt(&[visible.clone(), disabled, second.clone()]);
        assert!(out.contains("<name>visible</name>"));
        assert!(out.contains("<description>Use &lt;this&gt; &amp; that</description>"));
        assert!(out.contains("<location>/skills/visible/SKILL.md</location>"));
        assert!(out.contains("<name>second</name>"));
        assert!(!out.contains("hidden"));
        assert!(out.ends_with("</available_skills>"));

        let only_disabled = {
            let mut d = skill("hidden", "h", "/x");
            d.disable_model_invocation = Some(true);
            d
        };
        assert_eq!(format_skills_for_system_prompt(&[only_disabled]), "");
    }

    #[test]
    fn listing_escapes_all_fields() {
        let weird = skill(
            "a&b",
            "Quote \"double\" and 'single'",
            "/skills/<bad>&\"quote\"/SKILL.md",
        );
        let out = format_skills_for_system_prompt(&[weird]);
        assert!(out.contains("<name>a&amp;b</name>"));
        assert!(out.contains(
            "<description>Quote &quot;double&quot; and &apos;single&apos;</description>"
        ));
        assert!(
            out.contains("<location>/skills/&lt;bad&gt;&amp;&quot;quote&quot;/SKILL.md</location>")
        );
    }

    #[test]
    fn invocation_is_unescaped_and_appends_instructions() {
        let s = Skill {
            name: "example".to_string(),
            description: "desc".to_string(),
            content: "Do the <thing> & stuff".to_string(),
            file_path: "/skills/example/SKILL.md".to_string(),
            disable_model_invocation: None,
        };
        let out = format_skill_invocation(&s, None);
        assert!(out.starts_with("<skill name=\"example\" location=\"/skills/example/SKILL.md\">"));
        assert!(out.contains("References are relative to /skills/example."));
        assert!(out.contains("Do the <thing> & stuff"));

        let with_extra = format_skill_invocation(&s, Some("also do X"));
        assert!(with_extra.ends_with("also do X"));
        assert!(with_extra.contains("</skill>\n\nalso do X"));
    }

    #[test]
    fn validate_name_rules() {
        assert!(validate_name("example", "example").is_empty());
        assert!(!validate_name("Example", "example").is_empty()); // uppercase invalid
        assert!(!validate_name("ex--ample", "ex--ample").is_empty()); // consecutive hyphens
        assert!(!validate_name("-lead", "-lead").is_empty());
        assert!(!validate_name("wrong", "right").is_empty()); // mismatch with parent dir
    }

    #[test]
    fn validate_description_rules() {
        assert_eq!(validate_description(None).len(), 1);
        assert_eq!(validate_description(Some("   ")).len(), 1);
        assert!(validate_description(Some("ok")).is_empty());
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert_eq!(validate_description(Some(&long)).len(), 1);
    }

    #[test]
    fn prefix_ignore_pattern_strips_and_negates() {
        assert_eq!(
            prefix_ignore_pattern("node_modules", "pfx/"),
            Some("pfx/node_modules".into())
        );
        assert_eq!(
            prefix_ignore_pattern("!keep", "pfx/"),
            Some("!pfx/keep".into())
        );
        assert_eq!(
            prefix_ignore_pattern("/abs", "pfx/"),
            Some("pfx/abs".into())
        );
        assert_eq!(prefix_ignore_pattern("#comment", "pfx/"), None);
        assert_eq!(prefix_ignore_pattern("", "pfx/"), None);
    }

    #[test]
    fn dirname_and_relative_helpers() {
        assert_eq!(
            dirname_env_path("/skills/example/SKILL.md"),
            "/skills/example"
        );
        assert_eq!(dirname_env_path("C:\\proj\\SKILL.md"), "C:\\proj");
        assert_eq!(dirname_env_path("/SKILL.md"), "/");
        assert_eq!(
            relative_env_path("/skills", "/skills/example/SKILL.md"),
            "example/SKILL.md"
        );
        assert_eq!(relative_env_path("/skills", "/skills"), "");
    }

    #[test]
    fn ignore_matcher_basename_and_dir() {
        let mut m = IgnoreMatcher::new();
        m.add(&["foo.md".to_string()]); // basename match at any depth
        assert!(m.ignores("foo.md"));
        assert!(m.ignores("a/b/foo.md"));
        assert!(!m.ignores("bar.md"));

        let mut m2 = IgnoreMatcher::new();
        m2.add(&["build/".to_string()]); // dir-only
        assert!(m2.ignores("build/"));
        assert!(m2.ignores("build/x.txt"));
        assert!(!m2.ignores("buildx"));

        let mut m3 = IgnoreMatcher::new();
        m3.add(&["*.log".to_string()]);
        assert!(m3.ignores("a.log"));
        assert!(!m3.ignores("a.log/b")); // `*` doesn't cross `/`
    }
}
