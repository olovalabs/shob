//! User-editable settings (`<config>/settings.json`) and window geometry.
//!
//! Settings are loaded synchronously before the window is created so the saved
//! bounds can be restored. The file is parsed with `jsonc-parser`, so comments
//! and trailing commas are tolerated even though Shob writes plain JSON.

use crate::agent_auth::AgentModel;
use crate::paths;
use crate::theme::{PaletteScheme, ThemeColors, ThemePreference};
use gpui::{Bounds, Pixels, Size, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const DEFAULT_WINDOW_WIDTH: f32 = 1200.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 780.0;
const MIN_WINDOW_WIDTH: f32 = 400.0;
const MIN_WINDOW_HEIGHT: f32 = 300.0;

pub fn minimum_window_size() -> Size<Pixels> {
    size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))
}

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    #[serde(default)]
    pub state: WindowState,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for SavedWindow {
    fn default() -> Self {
        Self {
            state: WindowState::Windowed,
            x: 0.0,
            y: 0.0,
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        }
    }
}

impl SavedWindow {
    pub fn from_bounds(bounds: WindowBounds) -> Self {
        let inner = bounds.get_bounds();
        let state = match bounds {
            WindowBounds::Windowed(_) => WindowState::Windowed,
            WindowBounds::Maximized(_) => WindowState::Maximized,
            WindowBounds::Fullscreen(_) => WindowState::Fullscreen,
        };
        Self {
            state,
            x: f32::from(inner.origin.x),
            y: f32::from(inner.origin.y),
            width: f32::from(inner.size.width),
            height: f32::from(inner.size.height),
        }
    }

    fn is_usable(&self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.x.is_finite()
            && self.y.is_finite()
            && self.width >= MIN_WINDOW_WIDTH
            && self.height >= MIN_WINDOW_HEIGHT
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedSidebar {
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default = "default_sidebar_width")]
    pub width: f32,
}

impl Default for SavedSidebar {
    fn default() -> Self {
        Self {
            visible: true,
            width: default_sidebar_width(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RestoreOptions {
    #[serde(default = "default_true")]
    pub sessions: bool,
    #[serde(default = "default_true")]
    pub scrollback: bool,
    #[serde(default = "default_true")]
    pub agent_history: bool,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            sessions: true,
            scrollback: true,
            agent_history: true,
        }
    }
}

/// Idle-session resource policy.
///
/// Defaults reclaim only sessions the person is not looking at; hiding a pane
/// never kills its shell. Setting `hibernation` to false restores the old
/// resident model where every grid stays in memory until the pane is closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySettings {
    pub hibernation: bool,
    /// Seconds a quiescent pane waits before releasing disposable caches.
    pub idle_after_secs: u64,
    /// Seconds of no input or output before its grid is dropped.
    pub hibernate_after_secs: u64,
    /// In-memory scrollback for a live grid. A hibernated grid keeps none.
    pub terminal_scrollback_lines: usize,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            hibernation: true,
            idle_after_secs: 0,
            hibernate_after_secs: 900,
            terminal_scrollback_lines: 10_000,
        }
    }
}

impl MemorySettings {
    /// `SHOB_IDLE_SECS`, `SHOB_HIBERNATE_SECS` and `SHOB_DISABLE_HIBERNATION`
    /// override the file for one run, as documented in the README.
    pub fn policy(&self) -> crate::session_lifecycle::LifecyclePolicy {
        let policy = crate::session_lifecycle::LifecyclePolicy::from_lookup(
            |key| std::env::var(key).ok(),
            Duration::from_secs(self.idle_after_secs),
            Duration::from_secs(self.hibernate_after_secs),
        );
        crate::session_lifecycle::LifecyclePolicy {
            enabled: policy.enabled && self.hibernation,
            ..policy
        }
    }

    /// Hand-edited settings cannot ask for a grid that would exhaust memory or
    /// one so small that the shell's own startup output is lost.
    pub fn scrollback(&self) -> usize {
        self.terminal_scrollback_lines.clamp(1_000, 100_000)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub version: u32,
    pub theme: ThemePreference,
    pub palette_scheme: PaletteScheme,
    pub theme_colors: ThemeColors,
    pub default_agent_model: Option<AgentModel>,
    pub permission_mode: crate::agent_bridge::PermissionMode,
    #[serde(alias = "enabled_agent_models")]
    pub favorite_agent_models: Vec<AgentModel>,
    /// Explicit Shob execution opt-ins, scoped to canonical workspace paths.
    #[serde(alias = "trusted_omp_workspaces")]
    pub trusted_shob_workspaces: Vec<PathBuf>,
    pub sidebar: SavedSidebar,
    /// Show one centered agent conversation instead of the split workspace.
    pub agent_layout: bool,
    pub preferred_editor: Option<String>,
    pub project_scripts: std::collections::BTreeMap<PathBuf, Vec<ProjectScript>>,
    pub window: Option<SavedWindow>,
    pub restore: RestoreOptions,
    /// Token-reduction layer. Every field defaults to a working value, so an
    /// older `settings.json` without this key behaves as intended.
    pub token_efficiency: crate::token_efficiency::TokenEfficiencySettings,
    /// Idle-session reclamation. Missing in older files, so it defaults on.
    pub memory: MemorySettings,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectScript {
    pub name: String,
    pub command: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: 1,
            theme: ThemePreference::default(),
            palette_scheme: PaletteScheme::default(),
            theme_colors: ThemeColors::default(),
            default_agent_model: None,
            permission_mode: crate::agent_bridge::PermissionMode::Ask,
            favorite_agent_models: Vec::new(),
            trusted_shob_workspaces: Vec::new(),
            sidebar: SavedSidebar::default(),
            agent_layout: true,
            preferred_editor: None,
            project_scripts: Default::default(),
            window: None,
            restore: RestoreOptions::default(),
            token_efficiency: crate::token_efficiency::TokenEfficiencySettings::default(),
            memory: MemorySettings::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> f32 {
    f32::from(crate::SIDEBAR_WIDTH)
}

/// Parse JSONC (comments allowed) into `T`. Falls back to plain JSON so files
/// written by older versions still load.
fn parse_jsonc<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    let value = jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
        .map_err(|error| format!("invalid JSONC: {error}"))?;
    serde_json::from_value(value).map_err(|error| format!("invalid settings: {error}"))
}

fn load_from(path: &Path) -> Option<AppSettings> {
    let text = fs::read_to_string(path).ok()?;
    match parse_jsonc::<AppSettings>(&text) {
        Ok(settings) => Some(settings),
        Err(error) => {
            eprintln!("failed to parse {}: {error}", path.display());
            None
        }
    }
}

/// Load settings, migrating the pre-settings fields from a legacy workspace
/// file on first run.
pub fn load() -> AppSettings {
    let path = paths::settings_path();
    if path.exists() {
        return load_from(&path).unwrap_or_default();
    }

    let mut settings = AppSettings::default();
    if let Some(legacy) = load_legacy_settings() {
        settings.theme = legacy.theme.unwrap_or_default();
        settings.palette_scheme = legacy.palette_scheme.unwrap_or_default();
        settings.default_agent_model = legacy.default_agent_model;
        settings.sidebar.visible = legacy.sidebar_visible.unwrap_or(true);
        settings.sidebar.width = legacy
            .sidebar_width
            .filter(|width| width.is_finite() && *width > 0.0)
            .unwrap_or_else(default_sidebar_width);
    }
    settings.theme_colors = ThemeColors::complete_defaults(settings.palette_scheme);
    if let Err(error) = save_atomic(&settings) {
        eprintln!("failed to write initial settings: {error}");
    }
    settings
}

#[derive(Deserialize)]
struct LegacySettings {
    #[serde(default)]
    theme: Option<ThemePreference>,
    #[serde(default)]
    palette_scheme: Option<PaletteScheme>,
    #[serde(default)]
    default_agent_model: Option<AgentModel>,
    #[serde(default)]
    sidebar_visible: Option<bool>,
    #[serde(default)]
    sidebar_width: Option<f32>,
}

fn load_legacy_settings() -> Option<LegacySettings> {
    for path in paths::legacy_workspace_paths() {
        if !path.exists() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) =
            jsonc_parser::parse_to_serde_value(&text, &jsonc_parser::ParseOptions::default())
        else {
            continue;
        };
        if let Ok(legacy) = serde_json::from_value::<LegacySettings>(value) {
            return Some(legacy);
        }
    }
    None
}

pub fn save_atomic(settings: &AppSettings) -> io::Result<()> {
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    save_unlocked(settings)
}

fn save_unlocked(settings: &AppSettings) -> io::Result<()> {
    let path = paths::settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temp, bytes)?;
    if let Err(error) = paths::atomic_replace(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Preserve theme edits made by an external editor during routine autosaves.
pub fn save_preserving_theme(settings: &AppSettings) -> io::Result<()> {
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = paths::settings_path();
    let mut merged = settings.clone();
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        let disk: AppSettings = parse_jsonc(&text).map_err(io::Error::other)?;
        merged.theme_colors = disk.theme_colors;
        merged.theme = disk.theme;
        merged.palette_scheme = disk.palette_scheme;
    }
    save_unlocked(&merged)
}

/// Read the user's file without silently replacing invalid settings with defaults.
pub fn reload() -> Result<AppSettings, String> {
    let path = paths::settings_path();
    let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    parse_jsonc(&text)
}

/// Convert persisted geometry into `WindowBounds`, falling back to a centered
/// default when the saved position is unusable or off-screen.
pub fn window_bounds(saved: Option<SavedWindow>, displays: &[Bounds<Pixels>]) -> WindowBounds {
    let default_size = size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT));
    let primary = displays.first().copied();
    let centered = |size: Size<Pixels>| -> Bounds<Pixels> {
        match primary {
            Some(display) => Bounds::centered_at(display.center(), size),
            None => Bounds {
                origin: point(px(0.0), px(0.0)),
                size,
            },
        }
    };

    let Some(saved) = saved.filter(SavedWindow::is_usable) else {
        return WindowBounds::Windowed(centered(default_size));
    };

    let size = size(px(saved.width), px(saved.height));
    let origin = point(px(saved.x), px(saved.y));
    let bounds = Bounds { origin, size };
    let center = bounds.center();
    let on_screen = displays.is_empty() || displays.iter().any(|display| display.contains(&center));
    let bounds = if on_screen { bounds } else { centered(size) };

    match saved.state {
        WindowState::Windowed => WindowBounds::Windowed(bounds),
        WindowState::Maximized => WindowBounds::Maximized(bounds),
        WindowState::Fullscreen => WindowBounds::Fullscreen(bounds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(origin_x: f32, origin_y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(origin_x), px(origin_y)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn settings_roundtrip() {
        let settings = AppSettings {
            agent_layout: true,
            preferred_editor: Some("Zed".into()),
            project_scripts: [(
                PathBuf::from("/tmp/project"),
                vec![ProjectScript {
                    name: "Test".into(),
                    command: "cargo test".into(),
                }],
            )]
            .into(),
            permission_mode: crate::agent_bridge::PermissionMode::Full,
            theme: ThemePreference::Dark,
            palette_scheme: PaletteScheme::Github,
            trusted_shob_workspaces: vec![PathBuf::from("/tmp/explicit-workspace")],
            sidebar: SavedSidebar {
                visible: false,
                width: 300.0,
            },
            window: Some(SavedWindow {
                state: WindowState::Maximized,
                x: 10.0,
                y: 20.0,
                width: 900.0,
                height: 700.0,
            }),
            restore: RestoreOptions {
                sessions: true,
                scrollback: false,
                agent_history: true,
            },
            ..Default::default()
        };
        let text = serde_json::to_string(&settings).unwrap();
        let restored: AppSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(settings, restored);
    }

    #[test]
    fn legacy_settings_never_grant_shob_trust() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"version":1,"theme":"dark"}"#).unwrap();
        assert!(settings.trusted_shob_workspaces.is_empty());
        assert_eq!(
            settings.permission_mode,
            crate::agent_bridge::PermissionMode::Ask
        );
        assert!(AppSettings::default().trusted_shob_workspaces.is_empty());
    }

    #[test]
    fn legacy_workspace_trust_key_is_read_and_renamed_on_save() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"trusted_omp_workspaces":["/tmp/explicit-workspace"]}"#)
                .unwrap();
        assert_eq!(
            settings.trusted_shob_workspaces,
            vec![PathBuf::from("/tmp/explicit-workspace")]
        );
        let saved = serde_json::to_value(settings).unwrap();
        assert!(saved.get("trusted_shob_workspaces").is_some());
        assert!(saved.get("trusted_omp_workspaces").is_none());
    }

    #[test]
    fn jsonc_comments_are_accepted() {
        let text = r#"{
            // appearance
            "theme": "dark",
            "palette_scheme": "github",
            "sidebar": { "visible": false, "width": 320.0 },
        }"#;
        let settings: AppSettings = parse_jsonc(text).unwrap();
        assert_eq!(settings.theme, ThemePreference::Dark);
        assert_eq!(settings.palette_scheme, PaletteScheme::Github);
        assert!(!settings.sidebar.visible);
        assert_eq!(settings.sidebar.width, 320.0);
    }

    #[test]
    fn defaults_restore_everything() {
        let settings = AppSettings::default();
        assert!(settings.restore.agent_history);
        assert!(settings.restore.scrollback);
        assert!(settings.restore.sessions);
        assert!(settings.sidebar.visible);
        assert!(settings.agent_layout);
        assert!(parse_jsonc::<AppSettings>("{}").unwrap().agent_layout);
        assert!(
            !parse_jsonc::<AppSettings>(r#"{"agent_layout": false}"#)
                .unwrap()
                .agent_layout
        );
    }

    #[test]
    fn offscreen_window_is_centered() {
        let displays = vec![display(0.0, 0.0, 1920.0, 1080.0)];
        let saved = SavedWindow {
            state: WindowState::Windowed,
            x: 5000.0,
            y: 5000.0,
            width: 800.0,
            height: 600.0,
        };
        let bounds = window_bounds(Some(saved), &displays);
        let inner = bounds.get_bounds();
        assert_eq!(inner.size, size(px(800.0), px(600.0)));
        assert_eq!(inner.center(), displays[0].center());
    }

    #[test]
    fn tiny_window_falls_back_to_default_size() {
        let displays = vec![display(0.0, 0.0, 1920.0, 1080.0)];
        let saved = SavedWindow {
            state: WindowState::Windowed,
            x: 10.0,
            y: 10.0,
            width: 10.0,
            height: 10.0,
        };
        let inner = window_bounds(Some(saved), &displays).get_bounds();
        assert_eq!(
            inner.size,
            size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT))
        );
    }

    #[test]
    fn window_state_is_preserved() {
        let displays = vec![display(0.0, 0.0, 1920.0, 1080.0)];
        let saved = SavedWindow {
            state: WindowState::Maximized,
            x: 100.0,
            y: 100.0,
            width: 900.0,
            height: 700.0,
        };
        assert!(matches!(
            window_bounds(Some(saved), &displays),
            WindowBounds::Maximized(_)
        ));
    }
}

#[cfg(test)]
mod memory_settings_tests {
    use super::*;

    #[test]
    fn memory_defaults_reclaim_by_default_without_shrinking_scrollback() {
        let settings = AppSettings::default();
        assert!(settings.memory.hibernation);
        assert_eq!(settings.memory.hibernate_after_secs, 900);
        assert_eq!(settings.memory.scrollback(), 10_000);
        // The mapping itself (and its environment overrides) is tested in
        // `session_lifecycle`; here only the persisted defaults matter, because
        // the test process may carry a developer's `SHOB_*` overrides.
    }

    #[test]
    fn hand_edited_scrollback_is_clamped() {
        let mut memory = MemorySettings::default();
        memory.terminal_scrollback_lines = 10;
        assert_eq!(memory.scrollback(), 1_000);
        memory.terminal_scrollback_lines = 10_000_000;
        assert_eq!(memory.scrollback(), 100_000);
    }

    #[test]
    fn an_older_settings_file_gains_defaults() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"version":1,"agent_layout":false}"#).unwrap();
        assert_eq!(settings.memory, MemorySettings::default());
        assert!(!settings.agent_layout);
    }

    #[test]
    fn disabling_hibernation_keeps_the_rest_of_the_policy() {
        let mut memory = MemorySettings::default();
        memory.hibernation = false;
        memory.hibernate_after_secs = 120;
        assert!(!memory.policy().enabled);
    }
}
