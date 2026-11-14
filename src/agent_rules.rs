//! Agent detection driven by TOML rule packs.
//!
//! Upstream: <https://github.com/herdrdev/herdr>, `distribution/agent-detection`.
//! The packs under `assets/agent-detection/` are copied unmodified (Apache-2.0,
//! see the `NOTICE.md` beside them) and this module is a port of herdr's
//! `src/detect/manifest.rs`, so a pack keeps the exact meaning it has upstream.
//! Adding support for a new agent — or updating an existing one — is a TOML
//! change, not a Rust change.
//!
//! ## Semantics
//!
//! A rule *matches* only when **every** clause on it holds:
//!
//! * every `contains` needle appears in the region text (case-insensitively —
//!   needles are lowercased at load time),
//! * every `regex` matches somewhere in the region,
//! * every `line_regex` matches at least one single line of the region,
//! * every `all` gate matches,
//! * at least one `any` gate matches (when `any` is non-empty),
//! * and **no** `not` gate matches.
//!
//! Rules are then resolved by `priority`: the highest wins, and on a tie the
//! rule that appears *first in the file* is kept. When a pack is selected but
//! no rule matches, herdr reports `idle` for a known agent, which is what
//! [`evaluate`] returns via `None` + [`RulePack::is_known_agent`].
//!
//! ## Regions
//!
//! A rule reads a *slice* of the screen rather than the whole thing, so that
//! stale transcript text cannot satisfy a live rule. All specs used by the
//! bundled packs are implemented: `osc_title`, `osc_progress`, `whole_recent`,
//! `bottom_non_empty_lines(N)`, `bottom_lines(N)`, `top_non_empty_lines(N)`,
//! `after_last_prompt_marker`, `before_current_prompt_marker`,
//! `whole_recent_without_current_prompt_marker`, `prompt_box_body`,
//! `above_prompt_box`, `last_non_empty_above_prompt_box`, and
//! `after_last_horizontal_rule`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::assets::AppAssets;

/// Guard rails mirroring upstream's, so a hand-edited pack cannot blow the
/// stack or wedge the poller.
const MAX_GATE_DEPTH: usize = 8;
const MAX_RULES_PER_PACK: usize = 128;

/// The inputs a region can be sliced from.
#[derive(Debug, Clone, Copy, Default)]
pub struct DetectionInput<'a> {
    /// The visible screen as newline-separated text.
    pub screen: &'a str,
    /// The `OSC 0`/`OSC 2` window title.
    pub osc_title: &'a str,
    /// The `OSC 9;4` progress payload. Empty until the terminal parses it.
    pub osc_progress: &'a str,
}

/// The states a rule pack can report. Rule packs have no `error` state; failures are
/// handled by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleState {
    Idle,
    Working,
    Blocked,
    /// Neither working nor blocked, and not a definite "at rest" reading.
    Unknown,
}

impl RuleState {
    pub fn label(self) -> &'static str {
        match self {
            RuleState::Idle => "idle",
            RuleState::Working => "working",
            RuleState::Blocked => "blocked",
            RuleState::Unknown => "unknown",
        }
    }
}

/// A rule that matched, with the evidence needed to explain the badge.
#[derive(Debug, Clone)]
pub struct RuleMatch {
    /// Pack the rule came from, e.g. `claude`.
    pub pack: String,
    /// The pack's own revision from its TOML `version` field.
    pub pack_version: Option<String>,
    pub source: String,
    /// Rule id, e.g. `live_blocked_form`.
    pub rule: String,
    pub state: RuleState,
    pub priority: i32,
    pub region: String,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    /// The rule matched only a transient overlay; leave the reported state
    /// alone rather than flapping it.
    pub skip_state_update: bool,
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawManifest {
    id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    min_engine_version: Option<u32>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

/// A rule's matcher clauses, without the rule-level metadata.
///
/// Kept as its own type so `compile_gate` can recurse over `all`/`any`/`not`.
#[derive(Debug, Deserialize, Default, Clone)]
struct RawGate {
    #[serde(default)]
    all: Vec<RawGate>,
    #[serde(default)]
    any: Vec<RawGate>,
    #[serde(default, rename = "not")]
    not: Vec<RawGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    id: String,
    #[serde(default)]
    state: Option<RawState>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(default)]
    all: Vec<RawGate>,
    #[serde(default)]
    any: Vec<RawGate>,
    #[serde(default, rename = "not")]
    not: Vec<RawGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

impl RawRule {
    fn as_gate(&self) -> RawGate {
        RawGate {
            all: self.all.clone(),
            any: self.any.clone(),
            not: self.not.clone(),
            contains: self.contains.clone(),
            regex: self.regex.clone(),
            line_regex: self.line_regex.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RawState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<RawState> for RuleState {
    fn from(value: RawState) -> Self {
        match value {
            RawState::Idle => RuleState::Idle,
            RawState::Working => RuleState::Working,
            RawState::Blocked => RuleState::Blocked,
            RawState::Unknown => RuleState::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not: Vec<CompiledGate>,
    /// Already lowercased, matching how upstream compiles them.
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    id: String,
    state: RuleState,
    priority: i32,
    region: String,
    visible_idle: bool,
    visible_blocker: bool,
    visible_working: bool,
    skip_state_update: bool,
    gate: CompiledGate,
}

/// One agent's rule pack.
#[derive(Debug, Clone)]
pub struct RulePack {
    /// Canonical id, e.g. `claude`, `agy`, `copilot`.
    pub id: String,
    pub version: Option<String>,
    source: String,
    /// Every name this pack answers to, lowercased: its `id`, its `aliases`,
    /// and its file stem (which covers `antigravity` -> `agy`).
    names: Vec<String>,
    rules: Vec<CompiledRule>,
}

impl RulePack {
    fn parse(src: &str, path: &str, source: &str) -> Result<RulePack, String> {
        let raw: RawManifest = toml::from_str(src).map_err(|err| err.to_string())?;
        if raw.min_engine_version.unwrap_or(0) > 3 {
            return Err(format!(
                "manifest requires engine version {}, current engine is 3",
                raw.min_engine_version.unwrap_or(0)
            ));
        }
        if raw.rules.len() > MAX_RULES_PER_PACK {
            return Err(format!(
                "{} rules exceeds the {MAX_RULES_PER_PACK} rule limit",
                raw.rules.len()
            ));
        }

        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .trim_end_matches(".toml");
        let mut names = vec![raw.id.to_ascii_lowercase(), stem.to_ascii_lowercase()];
        names.extend(raw.aliases.iter().map(|alias| alias.to_ascii_lowercase()));
        names.retain(|name| !name.is_empty());
        names.sort();
        names.dedup();

        let mut rules = raw
            .rules
            .iter()
            .map(|rule| {
                let gate = compile_gate(&rule.as_gate(), 0)
                    .map_err(|err| format!("rule {} could not be compiled: {err}", rule.id))?;
                Ok(CompiledRule {
                    id: rule.id.clone(),
                    state: rule
                        .state
                        .map(RuleState::from)
                        .unwrap_or(RuleState::Unknown),
                    priority: rule.priority,
                    region: rule
                        .region
                        .clone()
                        .unwrap_or_else(|| "whole_recent".to_string()),
                    visible_idle: rule.visible_idle,
                    visible_blocker: rule.visible_blocker,
                    visible_working: rule.visible_working,
                    skip_state_update: rule.skip_state_update,
                    gate,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        // Highest priority first. The sort is stable, so file order still
        // breaks priority ties — exactly the resolution `evaluate` used to
        // compute per tick, now paid once at load so evaluation can stop at
        // the first match.
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));

        Ok(RulePack {
            id: raw.id,
            version: raw.version,
            source: source.to_owned(),
            names,
            rules,
        })
    }
}

/// Process names accepted that the packs do not spell
/// out themselves. Everything else (`claude-code`, `cursor-agent`, `ghcs`, …)
/// is already an `alias` inside the pack that owns it.
const EXTRA_PROCESS_ALIASES: &[(&str, &str)] = &[("opencode2", "opencode"), (".cline", "cline")];

fn normalize_name(name: &str) -> String {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Pack set
// ---------------------------------------------------------------------------

/// Every rule pack the binary carries.
#[derive(Debug)]
pub struct RuleSet {
    packs: Vec<RulePack>,
    /// Every name any pack answers to (ids, aliases, file stems, plus
    /// [`EXTRA_PROCESS_ALIASES`]) mapped to its pack index, so process-name
    /// lookups are one hash probe instead of a linear scan over all packs.
    by_name: HashMap<String, usize>,
}

fn load_manifest_directory(
    directory: &std::path::Path,
    source: &str,
    packs: &mut Vec<RulePack>,
    always_override: bool,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("toml") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let display = path.to_string_lossy();
        let Ok(pack) = RulePack::parse(&src, &display, source) else {
            continue;
        };
        if let Some(index) = packs.iter().position(|old| old.id == pack.id) {
            if always_override
                || version_key(pack.version.as_deref())
                    > version_key(packs[index].version.as_deref())
            {
                packs[index] = pack;
            }
        } else {
            packs.push(pack);
        }
    }
}

fn version_key(version: Option<&str>) -> Vec<u64> {
    version
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

static EMBEDDED: LazyLock<RuleSet> = LazyLock::new(RuleSet::load_embedded);

/// The process-wide pack set, parsed once on first use.
pub fn embedded() -> &'static RuleSet {
    &EMBEDDED
}

pub fn validate_manifest(src: &str, path: &str) -> Result<(String, Option<String>), String> {
    RulePack::parse(src, path, "validation").map(|pack| (pack.id, pack.version))
}

impl RuleSet {
    fn load_embedded() -> RuleSet {
        let mut packs = Vec::new();
        for path in AppAssets::iter() {
            let path = path.as_ref();
            if !path.starts_with("agent-detection/") || !path.ends_with(".toml") {
                continue;
            }
            if path.ends_with("index.toml") {
                continue;
            }
            let Some(file) = AppAssets::get(path) else {
                continue;
            };
            let src = String::from_utf8_lossy(&file.data);
            match RulePack::parse(&src, path, "bundled") {
                Ok(pack) => packs.push(pack),
                // One bad pack must not take detection down with it.
                Err(err) => eprintln!("[status] ignoring agent pack {path}: {err}"),
            }
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
            });
        if let Some(config) = config {
            let updates_dir = config.join("kathamo/agent-detection-updates");
            let updates_path = if updates_dir.exists() {
                updates_dir
            } else {
                config.join("markup/agent-detection-updates")
            };
            load_manifest_directory(&updates_path, "remote update", &mut packs, false);

            let override_dir = config.join("kathamo/agent-detection");
            let override_path = if override_dir.exists() {
                override_dir
            } else {
                config.join("markup/agent-detection")
            };
            load_manifest_directory(&override_path, "local override", &mut packs, true);
        }
        packs.sort_by(|a, b| a.id.cmp(&b.id));
        let mut by_name = HashMap::new();
        for (index, pack) in packs.iter().enumerate() {
            for name in &pack.names {
                by_name.insert(name.clone(), index);
            }
        }
        // Launcher variants recognized that no pack lists itself.
        for (alias, canonical) in EXTRA_PROCESS_ALIASES {
            if let Some(index) = packs.iter().position(|pack| pack.id == *canonical) {
                by_name.insert(alias.to_string(), index);
            }
        }
        RuleSet { packs, by_name }
    }

    /// The pack that owns any of `process_names`, first match in order.
    pub fn pack_for<'a>(&'a self, process_names: &[String]) -> Option<&'a RulePack> {
        process_names.iter().find_map(|name| {
            let key = normalize_name(name);
            self.by_name.get(&key).map(|&index| &self.packs[index])
        })
    }

    /// Canonical agent id for a process name, if a pack claims it.
    pub fn identify(&self, name: &str) -> Option<&str> {
        let key = normalize_name(name);
        self.by_name
            .get(&key)
            .map(|&index| self.packs[index].id.as_str())
    }

    /// Canonical agent id for an already-normalized name (lowercase basename,
    /// no path prefix): a bare hash lookup with no allocation, for hot paths
    /// that normalized the name themselves.
    pub fn identify_normalized(&self, key: &str) -> Option<&str> {
        self.by_name
            .get(key)
            .map(|&index| self.packs[index].id.as_str())
    }
}

/// Canonical agent id for a process name, e.g. `claude` for `/usr/bin/claude`.
///
/// Returns `None` for plain shells and unrecognized programs.
pub fn identify(name: &str) -> Option<&'static str> {
    embedded().identify(name)
}

/// Canonical agent id for an already-normalized name (see
/// [`RuleSet::identify_normalized`]).
pub fn identify_normalized(key: &str) -> Option<&'static str> {
    embedded().identify_normalized(key)
}

/// Evaluate one pack against the screen.
///
/// Rules arrive pre-sorted by descending priority (file order breaking ties),
/// so the first match wins and evaluation stops there. The screen is
/// lowercased once up front and each distinct region spec is sliced once —
/// packs reuse a handful of specs across all their rules, so this replaces up
/// to one full-screen allocation *per rule* per tick with a small constant.
pub fn evaluate(pack: &RulePack, input: DetectionInput<'_>) -> Option<RuleMatch> {
    // Slicing the lowered screen yields the same lines as lowering the sliced
    // region: lowercasing never adds or removes line breaks.
    let lower_screen = input.screen.to_lowercase();
    let lower_title = input.osc_title.to_lowercase();
    let lower_progress = input.osc_progress.to_lowercase();
    let lowered = DetectionInput {
        screen: &lower_screen,
        osc_title: &lower_title,
        osc_progress: &lower_progress,
    };
    // (region spec, region text, lowered region text), one entry per distinct
    // spec actually used. A tiny linear cache beats a HashMap here: packs use
    // only a few specs and this whole vec usually stays in one cache line.
    let mut regions: Vec<(&str, &str, &str)> = Vec::with_capacity(4);

    for rule in &pack.rules {
        let (region_text, lower_text) =
            match regions.iter().find(|(spec, _, _)| *spec == rule.region) {
                Some((_, text, lower)) => (*text, *lower),
                None => {
                    let text = region(input, &rule.region);
                    let lower = region(lowered, &rule.region);
                    regions.push((rule.region.as_str(), text, lower));
                    (text, lower)
                }
            };
        if !gate_matches(&rule.gate, region_text, lower_text) {
            continue;
        }
        return Some(RuleMatch {
            pack: pack.id.clone(),
            pack_version: pack.version.clone(),
            source: pack.source.clone(),
            rule: rule.id.clone(),
            state: rule.state,
            priority: rule.priority,
            region: rule.region.clone(),
            visible_idle: rule.visible_idle,
            visible_blocker: rule.visible_blocker,
            visible_working: rule.visible_working,
            skip_state_update: rule.skip_state_update,
        });
    }

    None
}

fn compile_gate(gate: &RawGate, depth: usize) -> Result<CompiledGate, String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("gate nesting exceeds {MAX_GATE_DEPTH} levels"));
    }
    let compile_all = |gates: &[RawGate]| -> Result<Vec<CompiledGate>, String> {
        gates
            .iter()
            .map(|nested| compile_gate(nested, depth + 1))
            .collect()
    };
    let compile_regexes = |patterns: &[String]| -> Result<Vec<Regex>, String> {
        patterns
            .iter()
            .map(|pattern| Regex::new(pattern).map_err(|err| err.to_string()))
            .collect()
    };

    Ok(CompiledGate {
        all: compile_all(&gate.all)?,
        any: compile_all(&gate.any)?,
        not: compile_all(&gate.not)?,
        contains: gate
            .contains
            .iter()
            .map(|needle| needle.to_lowercase())
            .collect(),
        regex: compile_regexes(&gate.regex)?,
        line_regex: compile_regexes(&gate.line_regex)?,
    })
}

/// Port of upstream's `compiled_gate_matches`.
fn gate_matches(gate: &CompiledGate, text: &str, lower_text: &str) -> bool {
    if !gate
        .contains
        .iter()
        .all(|needle| lower_text.contains(needle))
    {
        return false;
    }

    if !gate.regex.iter().all(|regex| regex.is_match(text)) {
        return false;
    }

    if !gate
        .line_regex
        .iter()
        .all(|regex| text.lines().any(|line| regex.is_match(line)))
    {
        return false;
    }

    if !gate
        .all
        .iter()
        .all(|nested| gate_matches(nested, text, lower_text))
    {
        return false;
    }

    if !gate.any.is_empty() && !gate.any.iter().any(|n| gate_matches(n, text, lower_text)) {
        return false;
    }

    if gate
        .not
        .iter()
        .any(|nested| gate_matches(nested, text, lower_text))
    {
        return false;
    }

    true
}

// ---------------------------------------------------------------------------
// Regions — ported from upstream's `region` and its helpers
// ---------------------------------------------------------------------------

fn region<'a>(input: DetectionInput<'a>, spec: &str) -> &'a str {
    let trimmed = spec.trim();
    // The OSC regions read their own field, not the screen.
    match trimmed {
        "osc_title" => return input.osc_title,
        "osc_progress" => return input.osc_progress,
        _ => {}
    }

    let content = input.screen;
    match trimmed {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "before_current_prompt_marker" => before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            whole_recent_without_current_prompt_marker(content)
        }
        "current_prompt_block_marker" => current_prompt_block_marker(content).unwrap_or(""),
        "after_current_prompt_block_marker" => {
            after_current_prompt_block_marker(content).unwrap_or("")
        }
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(content),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(trimmed, "bottom_lines") {
                return bottom_lines(content, count);
            }
            if let Some(count) = region_count(trimmed, "bottom_non_empty_lines") {
                return bottom_non_empty_lines(content, count);
            }
            if let Some(count) = top_region_count(trimmed) {
                return top_non_empty_lines(content, count);
            }
            // An unrecognized spec yields nothing, so the rule cannot match.
            ""
        }
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|count| count.parse::<usize>().ok())
}

fn top_region_count(spec: &str) -> Option<usize> {
    let count = spec
        .strip_prefix("top_non_empty_lines")?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if count.starts_with('0') || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    count
        .parse::<usize>()
        .ok()
        .filter(|count| *count <= u16::MAX as usize)
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(count);
    slice_from_line_index(content, &lines, start)
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start_index)
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(end_index) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    let byte_offset = line_start_offset(content, &lines, end_index + 1);
    &content[..byte_offset]
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = current_codex_prompt_index(&lines) else {
        return content;
    };
    let byte_offset = lines[..index]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>();
    &content[..byte_offset.min(content.len())]
}

fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    if current_codex_prompt_index(&lines).is_some() {
        ""
    } else {
        content
    }
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    lines[..prompt_index]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    let block_index = lines[..prompt_index]
        .iter()
        .rposition(|line| codex_block_marker_line(line))?;
    Some(slice_from_line_index(content, &lines, block_index))
}

/// Index of the prompt line the user is currently editing, if the newest
/// prompt has no block marker after it (which would mean it is in-flight).
fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt_index = lines.iter().rposition(|line| codex_prompt_line(line))?;
    if lines[prompt_index + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        return None;
    }
    Some(prompt_index)
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start.min(content.len())..end.min(content.len())])
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, top);
    &content[..end.min(content.len())]
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// The *top* border of the bottom-most boxed prompt, found by walking up and
/// taking the second horizontal rule.
fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

/// A box-drawing rule line: `────`, or `──── label` with at least three dashes.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }

    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();

    suffix.is_empty() || rule_chars >= 3
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    let byte_offset = line_start_offset(content, lines, index);
    &content[byte_offset.min(content.len())..]
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_input(screen: &str) -> DetectionInput<'_> {
        DetectionInput {
            screen,
            osc_title: "",
            osc_progress: "",
        }
    }

    #[test]
    fn every_embedded_pack_parses_and_compiles() {
        let set = embedded();
        // 22 agent packs ship in assets/agent-detection/.
        assert_eq!(set.packs.len(), 22, "expected all vendored packs to load");
        assert!(set.packs.iter().all(|pack| !pack.rules.is_empty()));
        // The packs we care most about must be present under their real ids.
        for id in ["claude", "codex", "gemini", "opencode", "cursor", "copilot"] {
            assert!(
                set.packs.iter().any(|pack| pack.id == id),
                "missing pack {id}"
            );
        }
    }

    #[test]
    fn pack_names_cover_ids_aliases_and_file_stems() {
        let set = embedded();
        // id
        assert_eq!(set.identify("claude"), Some("claude"));
        // alias
        assert_eq!(set.identify("claude-code"), Some("claude"));
        // file stem, where it differs from the id
        assert_eq!(set.identify("antigravity"), Some("agy"));
        assert_eq!(set.identify("github-copilot"), Some("copilot"));
        // path-qualified argv0
        assert_eq!(set.identify("/usr/local/bin/opencode"), Some("opencode"));
        // not agents
        assert_eq!(set.identify("fish"), None);
        assert_eq!(set.identify("sleep"), None);
    }

    #[test]
    fn regions_slice_the_way_upstream_does() {
        let screen = "one\n\ntwo\nthree\nfour";
        assert_eq!(region(screen_input(screen), "whole_recent"), screen);
        assert_eq!(
            region(screen_input(screen), "bottom_lines(2)"),
            "three\nfour"
        );
        assert_eq!(
            region(screen_input(screen), "bottom_non_empty_lines(2)"),
            "three\nfour"
        );
        assert_eq!(
            region(screen_input(screen), "bottom_non_empty_lines(4)"),
            screen
        );
        // Runs through to the end of the second non-empty line, blank lines
        // and all — matching upstream's offset arithmetic.
        assert_eq!(
            region(screen_input(screen), "top_non_empty_lines(2)"),
            "one\n\ntwo\n"
        );
        // OSC specs read their own field, never the screen.
        let input = DetectionInput {
            screen,
            osc_title: "⠋ Working",
            osc_progress: "4;0",
        };
        assert_eq!(region(input, "osc_title"), "⠋ Working");
        assert_eq!(region(input, "osc_progress"), "4;0");
        // Unknown specs are inert rather than matching everything.
        assert_eq!(region(screen_input(screen), "nonsense_region"), "");
    }

    #[test]
    fn claude_live_footer_is_working_not_blocked() {
        // `esc to interrupt` with an activity bullet is claude's live turn.
        let screen = "⏺ Read(src/main.rs)\n✳ Baking… (2m 14s · esc to interrupt)";
        let pack = embedded()
            .pack_for(&["claude".to_string()])
            .expect("claude pack");
        let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
        assert_eq!(m.state, RuleState::Working, "matched {}", m.rule);
    }

    #[test]
    fn claude_permission_menu_is_blocked() {
        let screen = "\
────────────────────────────
 Do you want to proceed?
 ❯ 1. Yes
   2. No, and tell Claude what to do differently
────────────────────────────
 esc to cancel · enter to confirm";
        let pack = embedded()
            .pack_for(&["claude".to_string()])
            .expect("claude pack");
        let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
        assert_eq!(m.state, RuleState::Blocked, "matched {}", m.rule);
    }

    #[test]
    fn gemini_working_hint_is_not_a_blocker() {
        // gemini ships `esc to cancel` as its *working* hint; the pack encodes
        // that directly, which is why the packs beat hand-rolled rules.
        let screen = " ⠋ Thinking... (esc to cancel)";
        let pack = embedded()
            .pack_for(&["gemini".to_string()])
            .expect("gemini pack");
        let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
        assert_eq!(m.state, RuleState::Working, "matched {}", m.rule);
    }

    #[test]
    fn unknown_region_makes_a_rule_inert() {
        // A rule pointing at a region we do not implement must never match.
        let pack = RulePack::parse(
            r#"
id = "test"
[[rules]]
id = "bogus"
state = "blocked"
priority = 100
region = "made_up_region"
contains = ["x"]
"#,
            "agent-detection/test.toml",
            "test",
        )
        .expect("pack parses");
        assert!(evaluate(&pack, screen_input("x")).is_none());
    }
}
