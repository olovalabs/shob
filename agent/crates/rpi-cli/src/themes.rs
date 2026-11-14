//! Theme spec resolution and loading.
//!
//! A *theme spec* is the value of `--theme` / `settings.json.theme`: either a
//! built-in preset name (`dark` / `light` / `monochrome`) or a path to a pi
//! theme JSON file (`ThemeJson`, see `rpi_tui::theme_json`). File specs expand
//! `~` and load lazily at the point of application so parse errors surface as
//! warnings plus a fallback to the built-in default, never a startup veto —
//! the same gentle style as `emit_startup_warnings`.
//!
//! `--no-themes` is enforced upstream (`app.rs` passes no theme at all), so
//! custom files cannot bypass it here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rpi_tui::theme::{global_theme_manager, Theme, ThemeManager, ThemePreset};
use rpi_tui::theme_json::ThemeJson;

/// A resolved theme spec: built-in preset or theme file path.
#[derive(Debug, Clone)]
pub enum ThemeSpec {
    Preset(ThemePreset),
    File(PathBuf),
}

/// Classify a theme spec string. Anything that is not a built-in preset name
/// is treated as a file path (the documented `--theme` semantics).
pub fn resolve_spec(value: &str) -> ThemeSpec {
    resolve_spec_in(value, None)
}

/// [`resolve_spec`] with a discovery registry: an exact registry name resolves
/// to its file, so a theme picked from the selector (persisted by name) loads
/// again on the next startup. Direct paths still win for values that are not
/// registry names.
pub fn resolve_spec_in(value: &str, registry: Option<&ThemeRegistry>) -> ThemeSpec {
    match value {
        "dark" => ThemeSpec::Preset(ThemePreset::Dark),
        "light" => ThemeSpec::Preset(ThemePreset::Light),
        "monochrome" => ThemeSpec::Preset(ThemePreset::Monochrome),
        other => {
            if let Some(path) = registry.and_then(|registry| registry.get(other)) {
                return ThemeSpec::File(path.clone());
            }
            ThemeSpec::File(expand_home(other))
        }
    }
}

/// Expand a leading `~` to the user's home directory. `~user` forms are left
/// untouched (only the current user's home is resolvable here).
pub fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

/// A successfully loaded file theme, ready to apply.
#[derive(Debug, Clone)]
pub struct LoadedTheme {
    /// Theme name from the JSON (`name` field), for messages.
    pub name: String,
    /// The parsed theme (default spacing/border styles).
    pub theme: Theme,
    /// The file it came from, for diagnostics.
    pub source: PathBuf,
}

impl LoadedTheme {
    /// Parse a pi theme file. The error message includes the path and reason.
    pub fn load(path: &Path) -> Result<Self, String> {
        let json = ThemeJson::from_file(path)
            .map_err(|e| format!("cannot load theme {}: {e}", path.display()))?;
        let theme = json
            .to_theme()
            .map_err(|e| format!("cannot load theme {}: {e}", path.display()))?;
        Ok(Self {
            name: json.name,
            theme,
            source: path.to_path_buf(),
        })
    }
}

/// Apply a file theme to both the process-wide theme (what components read)
/// and the session's theme manager (what `/theme` swaps), mirroring the
/// preset path's dual application.
pub fn apply_loaded(loaded: &LoadedTheme, session_manager: &ThemeManager) {
    global_theme_manager().set(loaded.theme.clone());
    session_manager.set(loaded.theme.clone());
}

/// Resolve a `--theme`/settings spec against the discovery registry and apply
/// it. Kept as one helper so the startup call site cannot resolve before the
/// registry is built (the saved value may be a registry name, which only
/// resolves when the registry is in scope).
pub fn apply_startup_spec(
    value: &str,
    registry: Option<&ThemeRegistry>,
    session_manager: &ThemeManager,
) -> Result<(), String> {
    apply_spec(&resolve_spec_in(value, registry), session_manager)
}

/// Resolve a `/theme <name>` argument to the spec `apply_and_persist`
/// consumes. Built-in presets are case-folded; discovered custom themes keep
/// the original case because the registry is case-sensitive and discovery
/// preserves file stems (`/theme Ocean` must find `Ocean.json`).
/// Returns `None` when the name matches neither a preset nor the registry.
pub fn resolve_command_theme(arg: &str, registry: Option<&ThemeRegistry>) -> Option<String> {
    let folded = arg.to_ascii_lowercase();
    let is_preset = matches!(folded.as_str(), "light" | "monochrome" | "dark");
    if is_preset {
        return Some(folded);
    }
    registry
        .and_then(|registry| registry.get(arg))
        .map(|path| path.to_string_lossy().into_owned())
}

/// Resolve + apply a theme spec at TUI startup.
///
/// Returns `Err(message)` when a file spec fails to load; the caller falls
/// back to the built-in default (no theme is applied) and surfaces the
/// message as a warning.
pub fn apply_spec(spec: &ThemeSpec, session_manager: &ThemeManager) -> Result<(), String> {
    match spec {
        ThemeSpec::Preset(preset) => {
            session_manager.apply_preset(*preset);
            rpi_tui::theme::apply_theme_preset(*preset);
            Ok(())
        }
        ThemeSpec::File(path) => match LoadedTheme::load(path) {
            Ok(loaded) => {
                apply_loaded(&loaded, session_manager);
                Ok(())
            }
            Err(message) => Err(message),
        },
    }
}

/// A registry of discovered custom theme files: display name → path.
///
/// Built once per TUI session by scanning the resource theme dirs; names are
/// file stems, first-wins on conflict (project `.rpi/themes` overrides the
/// global `~/.rpi/agent/themes`). Parsing is lazy: the registry only lists
/// files, and a theme is parsed when selected or applied, so a broken JSON
/// file cannot break the other themes.
#[derive(Debug, Clone, Default)]
pub struct ThemeRegistry {
    entries: BTreeMap<String, PathBuf>,
}

impl ThemeRegistry {
    /// Merge bare extension-contributed theme file paths (`resources_discover`
    /// `themePaths`) into this registry.
    ///
    /// Priority sits **after** the static dirs: a static dir theme of the same
    /// stem wins, extension themes fill the remaining names. This matches the
    /// skill/prompt precedence (static first-wins, discovered lower). Paths are
    /// concrete `.json` files (the pi contract, unlike skills' dir-or-file);
    /// names are file stems. v1 divergence (documented upstream parity gap):
    /// paths are unattributed, so diagnostics can only name the path.
    pub fn merge_discovered(&mut self, paths: &[String]) {
        for path in paths {
            let path = PathBuf::from(path);
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            self.entries.entry(name.to_string()).or_insert(path);
        }
    }

    /// Scan the global + project theme dirs. Project themes win on name
    /// conflicts. `project_trusted == false` skips the project dir entirely
    /// (the same trust boundary as skills: an untrusted repo must not shape
    /// the UI on first open).
    ///
    /// Returns the registry plus soft diagnostics for unreadable dirs.
    pub fn discover(cwd: &Path, project_trusted: bool) -> (Self, Vec<String>) {
        let global = crate::resource_dirs::global_dir("themes");
        let project = project_trusted.then(|| crate::resource_dirs::project_dir(cwd, "themes"));
        Self::discover_from(global.as_deref(), project.as_deref())
    }

    /// Scan explicit dirs (test seam; `discover` resolves the standard dirs).
    pub fn discover_from(global: Option<&Path>, project: Option<&Path>) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let mut entries = BTreeMap::new();
        if let Some(global) = global {
            scan_dir(global, &mut entries, &mut diagnostics, false);
        }
        if let Some(project) = project {
            scan_dir(project, &mut entries, &mut diagnostics, true);
        }
        (Self { entries }, diagnostics)
    }

    /// Look up a discovered theme by name (file stem, case-sensitive).
    pub fn get(&self, name: &str) -> Option<&PathBuf> {
        self.entries.get(name)
    }

    /// Discovered names in display order (alphabetical).
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Scan one theme directory into `entries`. `override_existing` is `true` for
/// the project dir (higher priority) and `false` for the global dir (first
/// wins within the pass, so two same-stem files in one dir keep the
/// alphabetically first).
fn scan_dir(
    dir: &Path,
    entries: &mut BTreeMap<String, PathBuf>,
    diagnostics: &mut Vec<String>,
    override_existing: bool,
) {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            diagnostics.push(format!("cannot read theme dir {}: {err}", dir.display()));
            return;
        }
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if override_existing {
            entries.insert(name.to_string(), path);
        } else {
            entries.entry(name.to_string()).or_insert(path);
        }
    }
}

/// Swap the theme watcher onto the current active spec.
///
/// File themes get an mtime watcher (poll → re-parse → apply → notify via the
/// `notify` callback); presets and unresolvable specs drop any existing watcher
/// (nothing to poll). The old handle is closed first so at most one poll task
/// runs. The caller owns the handle slot plus the current spec (this module
/// stays independent of the TUI state type); `registry` is the snapshot the
/// watcher re-applies through, and `session_manager` is the session's own
/// theme manager so a reload updates the same palette the UI reads — the
/// global manager is applied too, mirroring the normal selection path.
pub fn restart_theme_watcher<N>(
    slot: &Mutex<Option<crate::fs_watch::WatchHandle>>,
    active_spec: Option<&str>,
    registry: &ThemeRegistry,
    session_manager: &Arc<ThemeManager>,
    notify: N,
) where
    N: Fn(Result<String, String>) + Send + Sync + 'static,
{
    let path = active_spec.and_then(|spec| match resolve_spec_in(spec, Some(registry)) {
        ThemeSpec::File(path) if path.is_file() => Some(path),
        _ => None,
    });
    if let Some(handle) = slot.lock().ok().and_then(|mut guard| guard.take()) {
        handle.close();
    }
    if let Some(path) = path {
        let registry = registry.clone();
        let session_manager = session_manager.clone();
        let spec = active_spec.unwrap_or_default().to_string();
        let handle = crate::fs_watch::watch_path(path, move || {
            // Re-apply through the CURRENT registry so a name-persisted spec
            // follows the file the registry points at.
            let result = apply_and_persist(&spec, &session_manager, Some(&registry));
            notify(result);
        });
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(handle);
        }
    }
}

/// Reconcile a rebuilt registry after `/reload`: if the currently active file
/// theme (tracked as its spec value) is no longer resolvable, fall back to the
/// default preset and return the user-facing notice; otherwise leave the theme
/// alone and return `None`.
///
/// Resolution mirrors startup: registry name first (the persisted form for
/// discovered themes), then a direct path. A file that still exists keeps
/// working across reloads even though it came from a package that reloaded.
pub fn reconcile_after_reload(
    active_spec: Option<&str>,
    registry: &ThemeRegistry,
    session_manager: &ThemeManager,
) -> Option<String> {
    let active_spec = active_spec?;
    if active_spec == "dark" || active_spec == "light" || active_spec == "monochrome" {
        return None;
    }
    let still_resolvable = match resolve_spec_in(active_spec, Some(registry)) {
        ThemeSpec::File(path) => path.is_file(),
        ThemeSpec::Preset(_) => true,
    };
    if still_resolvable {
        return None;
    }
    session_manager.apply_preset(ThemePreset::Dark);
    rpi_tui::theme::apply_theme_preset(ThemePreset::Dark);
    Some(format!(
        "Theme \"{active_spec}\" is no longer available; reverted to the default dark theme."
    ))
}

/// Apply a theme spec (preset or file path) live and persist it.
///
/// Shared by `/theme <name>`, the `/theme` selector, and the settings panel:
/// applies to both managers, saves the spec string to `settings.json`, and
/// returns the user-facing message. Unknown names and load failures return
/// `Err` with the message to show.
pub fn apply_and_persist(
    spec_value: &str,
    session_manager: &ThemeManager,
    registry: Option<&ThemeRegistry>,
) -> Result<String, String> {
    let spec = resolve_spec_in(spec_value, registry);
    let label = match &spec {
        ThemeSpec::Preset(preset) => match preset {
            ThemePreset::Dark => "Dark".to_string(),
            ThemePreset::Light => "Light".to_string(),
            ThemePreset::Monochrome => "Monochrome".to_string(),
        },
        ThemeSpec::File(path) => {
            // Surface a load error before touching any state; the label is
            // the file stem (the discovery name), not the JSON `name` field.
            LoadedTheme::load(path).map_err(|message| message)?;
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("theme")
                .to_string()
        }
    };
    apply_spec(&spec, session_manager)?;
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    settings.theme = Some(spec_value.to_string());
    let saved = crate::settings::save_settings(&settings);
    Ok(format!(
        "Theme set to {label} (saved{})",
        if saved.is_ok() { "" } else { ", not saved" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::theme::ThemeColors;

    fn temp_theme_file(dir: &Path, name: &str, accent: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(r##"{{"name":"{name}","colors":{{"accent":"{accent}"}}}}"##),
        )
        .unwrap();
        path
    }

    #[test]
    fn merge_discovered_fills_after_static_dirs() {
        let root = std::env::temp_dir().join(format!("rpi-themes-ext-{}", std::process::id()));
        let project = root.join("proj/.rpi/themes");
        write_theme(&project, "shared.json");
        let ext_a = root.join("pkgA/themes/shared.json");
        let ext_b = root.join("pkgB/themes/extra.json");
        write_theme(&ext_a.parent().unwrap(), "shared.json");
        write_theme(&ext_b.parent().unwrap(), "extra.json");

        let (mut registry, _) = ThemeRegistry::discover_from(None, Some(&project));
        registry.merge_discovered(&[
            ext_a.to_string_lossy().to_string(),
            ext_b.to_string_lossy().to_string(),
            "/does/not/matter.md".to_string(), // non-json path is still keyed by stem
        ]);
        assert!(registry.get("shared").unwrap().starts_with(&project));
        assert!(registry.get("extra").unwrap().ends_with("extra.json"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reconcile_keeps_preset_and_existing_file() {
        let manager = ThemeManager::new();
        // Preset specs never fall back.
        assert!(
            reconcile_after_reload(Some("light"), &ThemeRegistry::default(), &manager).is_none()
        );
        // No active spec: nothing to do.
        assert!(reconcile_after_reload(None, &ThemeRegistry::default(), &manager).is_none());
        // An existing file theme stays.
        let dir = std::env::temp_dir().join(format!("rpi-themes-recon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "ocean.json", "#123456");
        assert!(reconcile_after_reload(
            Some(&path.to_string_lossy()),
            &ThemeRegistry::default(),
            &manager
        )
        .is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reconcile_falls_back_when_active_theme_vanishes() {
        let manager = ThemeManager::new();
        manager.apply_preset(ThemePreset::Light);
        let (registry, _) = ThemeRegistry::discover_from(None, None);
        let notice = reconcile_after_reload(Some("gone-theme"), &registry, &manager);
        assert!(notice.is_some());
        assert!(notice.unwrap().contains("gone-theme"));
        assert!(matches!(
            manager.get().colors.accent,
            rpi_tui::theme::Color::Ansi256(108)
        ));
    }

    #[test]
    fn restart_watcher_targets_files_only_and_swaps_handles() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!("rpi-theme-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "ocean.json", "#112233");
        let spec = path.to_string_lossy().to_string();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let slot: Mutex<Option<crate::fs_watch::WatchHandle>> = Mutex::new(None);
            let session_manager = Arc::new(ThemeManager::new());
            let hits = std::sync::Arc::new(AtomicUsize::new(0));
            let hits_ref = hits.clone();

            // File spec: a watcher is installed.
            let registry = ThemeRegistry::default();
            restart_theme_watcher(
                &slot,
                Some(&spec),
                &registry,
                &session_manager,
                move |_result| {
                    hits_ref.fetch_add(1, Ordering::SeqCst);
                },
            );
            assert!(slot.lock().unwrap().is_some());

            // Preset spec: the watcher is dropped.
            restart_theme_watcher(&slot, Some("dark"), &registry, &session_manager, |_| {});
            assert!(slot.lock().unwrap().is_none());

            // Re-install and edit the file: the callback fires (bounded wait;
            // the fs_watch poll is 1s) and re-applies through the registry.
            let hits_ref2 = hits.clone();
            restart_theme_watcher(
                &slot,
                Some(&spec),
                &registry,
                &session_manager,
                move |_result| {
                    hits_ref2.fetch_add(1, Ordering::SeqCst);
                },
            );
            // Let the spawned task take its baseline mtime snapshot before
            // writing, otherwise the edit lands in the baseline and never
            // registers as a change.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::fs::write(
                &path,
                r##"{"name":"ocean.json","colors":{"accent":"#445566"}}"##,
            )
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            while hits.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert!(hits.load(Ordering::SeqCst) >= 1, "mtime change must fire");
            // The reload must reach the session manager (what autocomplete and
            // /theme read), not only the process-wide one.
            assert_eq!(
                session_manager.get().colors.accent,
                rpi_tui::theme::Color::Rgb(68, 85, 102),
                "session manager must carry the reloaded palette"
            );
            assert!(
                slot.lock().unwrap().is_some(),
                "watcher survives after firing"
            );
            restart_theme_watcher(&slot, None, &registry, &session_manager, |_| {});
            assert!(slot.lock().unwrap().is_none());
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    // review_pr202: a spec persisted as a registry NAME must resolve through
    // the supplied registry (watch the discovered file), not be probed as a
    // cwd-relative path.
    #[test]
    fn review_pr202_watcher_follows_registry_names() {
        let root = std::env::temp_dir().join(format!("rpi-theme-watchreg-{}", std::process::id()));
        let project = root.join("proj/.rpi/themes");
        std::fs::create_dir_all(&project).unwrap();
        let path = temp_theme_file(&project, "ocean.json", "#112233");
        let (registry, _) = ThemeRegistry::discover_from(None, Some(&project));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let slot: Mutex<Option<crate::fs_watch::WatchHandle>> = Mutex::new(None);
            let session_manager = Arc::new(ThemeManager::new());
            restart_theme_watcher(
                &slot,
                Some("ocean"),
                &registry,
                &session_manager,
                move |_result| {},
            );
            assert!(
                slot.lock().unwrap().is_some(),
                "a registry-named theme must install a watcher"
            );
            // Control: with no registry the same name resolves to nothing and
            // installs no watcher (the pre-fix behavior).
            restart_theme_watcher(
                &slot,
                Some("ocean"),
                &ThemeRegistry::default(),
                &session_manager,
                move |_result| {},
            );
            assert!(slot.lock().unwrap().is_none());
        });
        let _ = path;
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn registry_name_resolves_through_resolve_spec_in() {
        let root = std::env::temp_dir().join(format!("rpi-themes-loop-{}", std::process::id()));
        let project = root.join("proj/.rpi/themes");
        let path = write_theme(&project, "ocean.json");
        let (registry, _) = ThemeRegistry::discover_from(None, Some(&project));

        let spec = resolve_spec_in("ocean", Some(&registry));
        assert!(matches!(spec, ThemeSpec::File(ref p) if *p == path));
        // Non-registry values still go through the path/tilde path.
        assert!(matches!(
            resolve_spec_in("other.json", Some(&registry)),
            ThemeSpec::File(_)
        ));
        assert!(matches!(
            resolve_spec_in("dark", Some(&registry)),
            ThemeSpec::Preset(ThemePreset::Dark)
        ));
        // apply_and_persist with the registry resolves the name to the file.
        let manager = ThemeManager::new();
        let message = apply_and_persist("ocean", &manager, Some(&registry)).unwrap();
        assert!(message.starts_with("Theme set to ocean"), "{message}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn apply_and_persist_preset_message_and_error() {
        let manager = ThemeManager::new();
        let message = apply_and_persist("light", &manager, None).unwrap();
        assert!(message.starts_with("Theme set to Light"), "{message}");
        assert!(message.contains("(saved"), "{message}");
        let err = apply_and_persist("no-such.json", &manager, None).unwrap_err();
        assert!(err.contains("no-such.json"), "{err}");
        // Failed apply leaves the previous theme intact.
        assert!(matches!(
            manager.get().colors.muted,
            rpi_tui::theme::Color::Ansi256(241)
        ));
    }

    #[test]
    fn apply_and_persist_file_theme_message_uses_stem() {
        let manager = ThemeManager::new();
        let dir = std::env::temp_dir().join(format!("rpi-theme-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "ocean.json", "#112233");
        let spec = path.to_string_lossy().to_string();
        let message = apply_and_persist(&spec, &manager, None).unwrap();
        assert!(message.starts_with("Theme set to ocean"), "{message}");
        assert_eq!(
            manager.get().colors.accent,
            rpi_tui::theme::Color::Rgb(17, 34, 51)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preset_names_classify_to_presets() {
        assert!(matches!(
            resolve_spec("dark"),
            ThemeSpec::Preset(ThemePreset::Dark)
        ));
        assert!(matches!(
            resolve_spec("light"),
            ThemeSpec::Preset(ThemePreset::Light)
        ));
        assert!(matches!(
            resolve_spec("monochrome"),
            ThemeSpec::Preset(ThemePreset::Monochrome)
        ));
        // Case-sensitive: preset names are lowercase identifiers.
        assert!(matches!(resolve_spec("Dark"), ThemeSpec::File(_)));
    }

    #[test]
    fn everything_else_is_a_file_path() {
        assert!(matches!(
            resolve_spec("themes/ocean.json"),
            ThemeSpec::File(ref p) if p == &PathBuf::from("themes/ocean.json")
        ));
        assert!(matches!(
            resolve_spec("~/.rpi/agent/themes/ocean.json"),
            ThemeSpec::File(ref p) if *p == expand_home("~/.rpi/agent/themes/ocean.json")
        ));
    }

    #[test]
    fn expand_home_forms() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/x.json"), home.join("x.json"));
        assert_eq!(expand_home("/tmp/x.json"), PathBuf::from("/tmp/x.json"));
        assert_eq!(expand_home("x.json"), PathBuf::from("x.json"));
        // ~user is not expanded (only the current user's home is resolvable).
        assert_eq!(expand_home("~root/x.json"), PathBuf::from("~root/x.json"));
    }

    #[test]
    fn load_file_theme_end_to_end() {
        let dir = std::env::temp_dir().join(format!("rpi-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "ocean.json", "#3366ff");
        let loaded = LoadedTheme::load(&path).unwrap();
        assert_eq!(loaded.name, "ocean.json");
        assert_eq!(
            loaded.theme.colors.accent,
            rpi_tui::theme::Color::Rgb(51, 102, 255)
        );
        // Everything else stays at the default palette.
        assert_eq!(
            loaded.theme.colors.md_heading,
            ThemeColors::default().md_heading
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_failure_message_contains_path_and_reason() {
        let dir = std::env::temp_dir().join(format!("rpi-theme-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.json");
        std::fs::write(&path, r#"{"name":"broken","colors":{"accent":"nope"}}"#).unwrap();
        let err = LoadedTheme::load(&path).unwrap_err();
        assert!(err.contains("broken.json"), "{err}");
        assert!(err.contains("nope"), "{err}");

        let missing = dir.join("missing.json");
        let err = LoadedTheme::load(&missing).unwrap_err();
        assert!(err.contains("missing.json"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_spec_preset_and_file_and_failure() {
        let manager = ThemeManager::new();
        let revision_before = rpi_tui::theme::theme_revision();

        // Preset applies to both managers.
        apply_spec(&ThemeSpec::Preset(ThemePreset::Light), &manager).unwrap();
        assert!(matches!(
            manager.get().colors.muted,
            rpi_tui::theme::Color::Ansi256(241)
        ));

        // File spec applies the parsed colors.
        let dir = std::env::temp_dir().join(format!("rpi-theme-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "ocean.json", "#ff0000");
        apply_spec(&ThemeSpec::File(path.clone()), &manager).unwrap();
        assert_eq!(
            manager.get().colors.accent,
            rpi_tui::theme::Color::Rgb(255, 0, 0)
        );
        assert_eq!(
            global_theme_manager().get().colors.accent,
            rpi_tui::theme::Color::Rgb(255, 0, 0)
        );

        // Failed file spec returns the message; managers keep the last theme.
        let err = apply_spec(&ThemeSpec::File(dir.join("missing.json")), &manager).unwrap_err();
        assert!(err.contains("missing.json"), "{err}");
        assert_eq!(
            manager.get().colors.accent,
            rpi_tui::theme::Color::Rgb(255, 0, 0)
        );

        assert!(rpi_tui::theme::theme_revision() > revision_before);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn write_theme(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!(r#"{{"name":"{}","colors":{{}}}}"#, name)).unwrap();
        path
    }

    #[test]
    fn discover_scans_global_and_project_dirs() {
        let root = std::env::temp_dir().join(format!("rpi-themes-disc-{}", std::process::id()));
        let global = root.join("global/themes");
        std::fs::create_dir_all(&global).unwrap();
        write_theme(&global, "ocean.json");
        write_theme(&global, "sunset.json");
        write_theme(&root.join("global/other"), "ignored.json"); // not scanned
        std::fs::write(global.join("notes.txt"), "not json").unwrap();

        let (registry, diagnostics) = ThemeRegistry::discover_from(Some(&global), None);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let names: Vec<_> = registry.names().cloned().collect();
        assert_eq!(names, vec!["ocean".to_string(), "sunset".to_string()]);
        assert!(!registry.is_empty());
        assert!(registry.get("ocean").unwrap().ends_with("ocean.json"));
        assert!(registry.get("missing").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn project_theme_overrides_global_same_name() {
        let root = std::env::temp_dir().join(format!("rpi-themes-ovr-{}", std::process::id()));
        let global = root.join("global/themes");
        let project = root.join("proj/.rpi/themes");
        std::fs::create_dir_all(&global).unwrap();
        write_theme(&global, "ocean.json");
        write_theme(&project, "ocean.json");

        let (registry, _) = ThemeRegistry::discover_from(Some(&global), Some(&project));
        assert_eq!(registry.len(), 1);
        let path = registry.get("ocean").unwrap();
        assert!(path.starts_with(&project), "{path:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn untrusted_project_dir_is_skipped() {
        let root = std::env::temp_dir().join(format!("rpi-themes-untrust-{}", std::process::id()));
        let cwd = root.join("proj");
        let project = cwd.join(".rpi/themes");
        write_theme(&project, "evil.json");

        // Untrusted: the project dir is never scanned, so evil.json cannot
        // appear regardless of what the global dir holds. Trusted: it does.
        // (The global dir resolves to the real agent dir; "evil" is unique to
        // this test's project, so the assertions are collision-free.)
        let (registry, _) = ThemeRegistry::discover(&cwd, false);
        assert!(registry.get("evil").is_none());
        let (registry, _) = ThemeRegistry::discover(&cwd, true);
        assert!(registry.get("evil").is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_dirs_are_silent_bad_dir_is_a_diagnostic() {
        let root = std::env::temp_dir().join(format!("rpi-themes-miss-{}", std::process::id()));
        let (registry, diagnostics) =
            ThemeRegistry::discover_from(None, Some(&root.join("proj/.rpi/themes")));
        assert!(registry.is_empty());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // A *file* where a theme dir should be is a soft diagnostic.
        let global = root.join("global/themes");
        std::fs::create_dir_all(&root.join("global")).unwrap();
        std::fs::write(&global, "not a dir").unwrap();
        let (registry, diagnostics) = ThemeRegistry::discover_from(Some(&global), None);
        assert!(registry.is_empty());
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics[0].contains("cannot read theme dir"),
            "{diagnostics:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn loaded_theme_load_parses_without_applying() {
        // Note: global-manager side effects cannot be observed reliably from a
        // parallel test suite (sibling tests share the process-wide manager),
        // so this only pins that loading produces the parsed colors; by
        // construction `LoadedTheme::load` touches no manager.
        let dir = std::env::temp_dir().join(format!("rpi-theme-prev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = temp_theme_file(&dir, "solo.json", "#00ff00");
        let loaded = LoadedTheme::load(&path).unwrap();
        assert_eq!(
            loaded.theme.colors.accent,
            rpi_tui::theme::Color::Rgb(0, 255, 0)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // review_pr200: the saved selector name (a registry name, not a cwd
    // relative path) must survive a restart once the registry is in scope.
    #[test]
    fn review_pr200_startup_saved_registry_name_round_trips() {
        let root = std::env::temp_dir().join(format!("rpi-theme-start-{}", std::process::id()));
        let project = root.join("proj/.rpi/themes");
        std::fs::create_dir_all(&project).unwrap();
        let path = temp_theme_file(&project, "ocean.json", "#112233");
        let (registry, _) = ThemeRegistry::discover_from(None, Some(&project));

        // Startup order contract: the registry exists before the saved spec
        // is resolved. `apply_startup_spec` takes the registry explicitly, so
        // the name resolves instead of degrading to a cwd-relative path.
        let manager = ThemeManager::new();
        apply_startup_spec("ocean", Some(&registry), &manager).unwrap();
        assert_eq!(
            manager.get().colors.accent,
            rpi_tui::theme::Color::Rgb(17, 34, 51)
        );

        // Control: without the registry the same value is a missing cwd file.
        let err = apply_startup_spec("ocean", None, &ThemeManager::new()).unwrap_err();
        assert!(err.contains("ocean"), "{err}");
        let _ = path;
        std::fs::remove_dir_all(&root).ok();
    }

    // review_pr200: /theme <name> keeps custom-name case for the registry
    // lookup (case-sensitive stems) while presets stay case-folded.
    #[test]
    fn review_pr200_command_theme_preserves_custom_case() {
        let root = std::env::temp_dir().join(format!("rpi-theme-case-{}", std::process::id()));
        let project = root.join("proj/.rpi/themes");
        write_theme(&project, "Ocean.json");
        let (registry, _) = ThemeRegistry::discover_from(None, Some(&project));

        let resolved = resolve_command_theme("Ocean", Some(&registry)).expect("Ocean resolves");
        assert!(resolved.ends_with("Ocean.json"), "{resolved}");
        // Folded lookup would miss the stem; pin that this is not what happens.
        assert!(registry.get("ocean").is_none());
        // Presets still fold, and unknown names return None.
        assert_eq!(resolve_command_theme("DARK", None).as_deref(), Some("dark"));
        assert!(resolve_command_theme("Nope", Some(&registry)).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
