use gpui::SharedString;
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::path::Path;
use std::sync::LazyLock;

use crate::agent_rules;
use crate::assets::AppAssets;
use crate::status::SessionStatus;

pub const DEFAULT_LOGO: &str = "icons/terminal.svg";

pub fn default_logo() -> SharedString {
    SharedString::from(DEFAULT_LOGO)
}

/// Registry of available logo assets mapped by command name / file stem.
#[derive(Debug)]
pub struct LogoRegistry {
    /// Mapping from lowercase command name (e.g. "claude", "openclaw", "agy") to relative asset path ("logos/claude.png").
    stems: HashMap<String, String>,
}

static REGISTRY: LazyLock<LogoRegistry> = LazyLock::new(LogoRegistry::discover);

pub fn registry() -> &'static LogoRegistry {
    &REGISTRY
}

impl LogoRegistry {
    pub fn discover() -> Self {
        let mut stems = HashMap::new();

        // 1. Scan embedded assets
        for path in AppAssets::iter() {
            let path_str = path.as_ref();
            if path_str.starts_with("logos/") || path_str.starts_with("assets/logos/") {
                let clean = path_str.trim_start_matches("assets/");
                if let Some(stem) = file_stem(clean) {
                    stems.insert(stem.to_ascii_lowercase(), clean.to_string());
                }
            }
        }

        // 2. Debug-only: scan the working-tree directory so newly added files
        // show up without a rebuild. Release builds use the embedded assets.
        #[cfg(debug_assertions)]
        {
            if let Ok(entries) = std::fs::read_dir(Path::new("assets/logos")) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            if let Some(file_name) = p.file_name().and_then(|s| s.to_str()) {
                                let rel = format!("logos/{file_name}");
                                stems.insert(stem.to_ascii_lowercase(), rel);
                            }
                        }
                    }
                }
            }
        }

        LogoRegistry { stems }
    }

    /// Total number of discovered logo stems.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.stems.len()
    }

    /// Whether any logos were registered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.stems.is_empty()
    }

    /// Finds a logo asset path for a command or agent name.
    ///
    /// The hot path uses [`Self::find_logo_normalized`] directly; this
    /// normalizing wrapper is exercised by the tests below.
    #[allow(dead_code)]
    pub fn find_logo(&self, name: &str) -> Option<&str> {
        let key = normalize_cmd_name(name);
        if key.is_empty() {
            return None;
        }
        self.find_logo_normalized(&key)
    }

    /// Like [`Self::find_logo`], but takes an already-normalized key
    /// (lowercase basename, see [`normalize_cmd_name`]) so hot paths that
    /// normalized the name themselves do not allocate per probe.
    pub fn find_logo_normalized(&self, key: &str) -> Option<&str> {
        if key.is_empty() {
            return None;
        }

        if matches!(key, "commandcode" | "command-code") {
            return self.stems.get("cmd").map(String::as_str);
        }

        // 1. Exact match on stem (e.g. "claude", "openclaw", "omp", "freebuff", "agy", "herdr", "cursor")
        if let Some(path) = self.stems.get(key) {
            return Some(path.as_str());
        }

        // 2. Match via agent detection manifests / aliases
        // (e.g. "antigravity" -> "agy", "claude-code" -> "claude", "cursor-agent" -> "cursor",
        //       "kilo-code" -> "kilo", "grok-build" -> "grok", "open-code" -> "opencode")
        if let Some(agent_id) = agent_rules::identify_normalized(key) {
            if let Some(path) = self.stems.get(agent_id) {
                return Some(path.as_str());
            }
        }

        // 3. Suffix-stripped name (e.g. "herdr-cli" -> "herdr", "pi-code" -> "pi")
        for suffix in ["-cli", "_cli", "-agent", "_agent", "-code", "_code"] {
            if let Some(stripped) = key.strip_suffix(suffix) {
                if let Some(path) = self.stems.get(stripped) {
                    return Some(path.as_str());
                }
                if let Some(agent_id) = agent_rules::identify_normalized(stripped) {
                    if let Some(path) = self.stems.get(agent_id) {
                        return Some(path.as_str());
                    }
                }
            }
        }

        None
    }
}

fn file_stem(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().filter(|s| !s.is_empty())
}

fn normalize_cmd_name(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name).trim();
    // Strip trailing script/binary extensions
    let without_ext = if let Some(idx) = base.rfind('.') {
        let ext = &base[idx + 1..];
        if ["exe", "bin", "sh", "py", "js", "mjs", "ts", "cjs"]
            .contains(&ext.to_ascii_lowercase().as_str())
        {
            &base[..idx]
        } else {
            base
        }
    } else {
        base
    };
    without_ext.to_ascii_lowercase()
}

/// Shells, wrappers, and interpreters that never identify a session on their own.
/// Compared against normalized (lowercase, extension-stripped) names.
const GENERIC_NAMES: &[&str] = &[
    "sh", "bash", "zsh", "fish", "sudo", "su", "env", "login", "node", "python", "python3",
];

fn is_generic_name(normalized: &str) -> bool {
    GENERIC_NAMES.contains(&normalized)
}

/// Resolves the logo asset path for a session given its current foreground processes,
/// title, and status.
pub fn resolve_session_logo(
    foreground: &[String],
    title: Option<&str>,
    status: SessionStatus,
    current_logo: &str,
) -> SharedString {
    let reg = registry();

    // Each foreground name is normalized once; the generic-shell check and the
    // logo lookups then share that form instead of allocating per probe.
    let mut lowered: Vec<String> = Vec::with_capacity(foreground.len());
    for name in foreground {
        lowered.push(normalize_cmd_name(name));
    }

    // Priority 1: non-generic process candidates
    for key in lowered.iter().filter(|key| !is_generic_name(key)) {
        if let Some(path) = reg.find_logo_normalized(key) {
            return SharedString::from(path.to_string());
        }
    }

    // Priority 2: all foreground process candidates
    for key in &lowered {
        if let Some(path) = reg.find_logo_normalized(key) {
            return SharedString::from(path.to_string());
        }
    }

    // 2. Check title (OSC title or terminal title)
    if let Some(title) = title {
        let clean = title.trim();
        // Check tokens in title
        for word in clean.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
            if word.is_empty() {
                continue;
            }
            let key = normalize_cmd_name(word);
            if key.is_empty() || is_generic_name(&key) {
                continue;
            }
            if let Some(path) = reg.find_logo_normalized(&key) {
                return SharedString::from(path.to_string());
            }
        }
    }

    // 3. Preserve agent logo while status is sticky Done
    if status == SessionStatus::Done && current_logo != DEFAULT_LOGO && !current_logo.is_empty() {
        return SharedString::from(current_logo.to_string());
    }

    // 4. Default fallback: a neutral terminal icon.
    default_logo()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_all_available_logos() {
        let reg = registry();
        assert!(reg.find_logo("claude").is_some());
        assert!(reg.find_logo("agy").is_some());
        assert!(reg.find_logo("cline").is_some());
        assert!(reg.find_logo("codex").is_some());
        assert!(reg.find_logo("cursor").is_some());
        assert!(reg.find_logo("freebuff").is_some());
        assert!(reg.find_logo("grok").is_some());
        assert!(reg.find_logo("herdr").is_some());
        assert!(reg.find_logo("kathamo").is_some());
        assert!(reg.find_logo("hermes").is_some());
        assert!(reg.find_logo("kilo").is_some());
        assert!(reg.find_logo("omp").is_some());
        assert!(reg.find_logo("openclaw").is_some());
        assert!(reg.find_logo("opencode").is_some());
        assert!(reg.find_logo("pi").is_some());
        assert!(reg.find_logo("cmd").is_some());
    }

    #[test]
    fn aliases_and_manifests_map_correctly() {
        let reg = registry();
        assert_eq!(reg.find_logo("antigravity"), Some("logos/agy.png"));
        assert_eq!(reg.find_logo("antigravity-cli"), Some("logos/agy.png"));
        assert_eq!(reg.find_logo("claude-code"), Some("logos/claude.png"));
        assert_eq!(reg.find_logo("open-code"), Some("logos/opencode.webp"));
        assert_eq!(reg.find_logo("opencode2"), Some("logos/opencode.webp"));
        assert_eq!(reg.find_logo("cursor-agent"), Some("logos/cursor.png"));
        assert_eq!(reg.find_logo("kilo-code"), Some("logos/kilo.png"));
        assert_eq!(reg.find_logo("hermes-agent"), Some("logos/hermes.png"));
        assert_eq!(reg.find_logo("grok-build"), Some("logos/grok.png"));
        assert_eq!(reg.find_logo(".cline"), Some("logos/cline.png"));
        assert_eq!(reg.find_logo("command-code"), Some("logos/cmd.png"));
        assert_eq!(reg.find_logo("commandcode"), Some("logos/cmd.png"));
    }

    #[test]
    fn resolution_prefers_active_agent_over_generic_shell() {
        let fg = vec!["bash".to_string(), "claude".to_string()];
        let logo = resolve_session_logo(&fg, None, SessionStatus::Working, DEFAULT_LOGO);
        assert_eq!(logo.as_ref(), "logos/claude.png");
    }

    #[test]
    fn resolution_falls_back_to_cmd_at_idle_prompt() {
        let fg: Vec<String> = vec![];
        let logo = resolve_session_logo(&fg, Some("terminal 1"), SessionStatus::Idle, DEFAULT_LOGO);
        assert_eq!(logo.as_ref(), DEFAULT_LOGO);
        assert_ne!(logo.as_ref(), "logos/cmd.png");
    }

    #[test]
    fn command_code_uses_its_own_logo_when_detected() {
        let fg = vec!["command-code".to_string()];
        let logo = resolve_session_logo(&fg, None, SessionStatus::Working, DEFAULT_LOGO);
        assert_eq!(logo.as_ref(), "logos/cmd.png");
    }

    #[test]
    fn resolution_preserves_logo_in_done_state() {
        let fg: Vec<String> = vec![];
        let logo = resolve_session_logo(
            &fg,
            Some("terminal 1"),
            SessionStatus::Done,
            "logos/claude.png",
        );
        assert_eq!(logo.as_ref(), "logos/claude.png");
    }

    #[test]
    fn resolution_from_title() {
        let fg: Vec<String> = vec![];
        let logo = resolve_session_logo(
            &fg,
            Some("Claude Code — project"),
            SessionStatus::Idle,
            DEFAULT_LOGO,
        );
        assert_eq!(logo.as_ref(), "logos/claude.png");

        let logo_agy =
            resolve_session_logo(&fg, Some("Antigravity"), SessionStatus::Idle, DEFAULT_LOGO);
        assert_eq!(logo_agy.as_ref(), "logos/agy.png");
    }
}
