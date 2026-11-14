//! Settings selectors, model catalog filtering, and setting changes.

use super::*;

pub(super) fn scoped_catalog(
    catalog: &[rpi_ai::Model],
    current: &rpi_ai::Model,
) -> Vec<rpi_ai::Model> {
    let scoped = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    if scoped.is_empty() {
        return catalog.to_vec();
    }
    let mut out: Vec<rpi_ai::Model> = catalog
        .iter()
        .filter(|m| {
            scoped.iter().any(|s| {
                s.eq_ignore_ascii_case(&m.id)
                    || s.eq_ignore_ascii_case(&format!("{}/{}", m.provider, m.id))
            })
        })
        .cloned()
        .collect();
    // Never strand the user: if the current model isn't in scope, keep it.
    if !out.contains(current) {
        out.push(current.clone());
    }
    out
}

/// Interactive `/settings` menu.
///
/// A [`SettingsList`] of editable settings (upstream's `SettingsSelectorComponent`):
/// Enter/Space cycles a row's value in place, and the theme/model/thinking/scope
/// rows open a sub-selector that replaces the active selector (the
/// `active_selector` slot is single). Value changes apply immediately where the
/// running TUI can honor them and are always persisted to `settings.json`, so
/// rows marked "applies on restart" take effect next launch.
pub(super) fn open_settings_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) {
    let settings = crate::settings::load_settings().unwrap_or_default();
    let items = settings_menu_items(&settings, state, lane_model_id);
    let list = Arc::new(SettingsList::new(items));

    // ---- submenu rows ----
    // These reuse the existing sub-selectors, which already persist their own
    // choices. The settings menu is replaced by the sub-selector (single
    // `active_selector` slot); cancelling the sub-selector returns to the editor.
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let lane_sel = lane.clone();
    let catalog_sel = catalog.to_vec();
    let lane_model_sel = lane_model_id.to_string();
    list.on_select(Arc::new(move |item| match item.key.as_str() {
        "theme" => open_settings_theme_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel),
        "model" => open_settings_model_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &lane_sel,
            &catalog_sel,
            &lane_model_sel,
        ),
        "thinking" => open_settings_thinking_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &lane_sel,
            &catalog_sel,
            &lane_model_sel,
        ),
        "scoped-models" => {
            open_scoped_models_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel, &catalog_sel)
        }
        _ => close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel),
    }));

    // ---- value rows ----
    // Apply live where possible, then persist so the choice survives a restart.
    let state_change = state.clone();
    let chat_change = state.notice_container();
    let tui_change = tui.clone();
    list.on_change(Arc::new(move |key, value| {
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        let applied = apply_setting_change(&state_change, key, value, &mut settings);
        let saved = crate::settings::save_settings(&settings);
        if let Some(note) = applied {
            let suffix = if saved.is_ok() { "" } else { " (not saved)" };
            add_note_message(&chat_change, &format!("{note}{suffix}"));
        }
        tui_change.request_render(false);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::Settings(list.clone()),
        list,
        SelectorKind::Settings,
    );
}

/// Build the `/settings` rows from the loaded settings plus the live TUI state.
///
/// Descriptions carry "(applies on restart)" for values resolved once at
/// startup, so the menu never implies an effect the running session cannot
/// apply.
pub(super) fn settings_menu_items(
    settings: &crate::settings::Settings,
    state: &Arc<TuiState>,
    lane_model_id: &str,
) -> Vec<SettingItem> {
    let current_theme = settings.theme.clone().unwrap_or_else(|| "dark".to_string());
    let current_model = settings
        .default_model
        .clone()
        .unwrap_or_else(|| lane_model_id.to_string());
    let current_thinking = settings
        .default_thinking_level
        .clone()
        .unwrap_or_else(|| "(default)".to_string());
    let scoped_desc = match &settings.scoped_models {
        Some(list) if !list.is_empty() => format!("{} model(s)", list.len()),
        _ => "all models".to_string(),
    };
    let hide_thinking = state.hide_thinking();
    let show_images = state.show_images.lock().map(|guard| *guard).unwrap_or(true);
    let images_supported = rpi_tui::get_capabilities().protocol != rpi_tui::ImageProtocol::None;
    let show_cache_miss = settings.show_cache_miss_notices.unwrap_or(false);
    let http_idle = settings
        .http_idle_timeout_ms()
        .map(|ms| {
            if ms == 0 {
                "disabled".to_string()
            } else {
                format!("{}s", ms / 1000)
            }
        })
        .unwrap_or_else(|| "5m".to_string());

    vec![
        SettingItem::new("theme", "Theme", &current_theme)
            .with_description("Color theme for the interface")
            .with_submenu(),
        SettingItem::new("model", "Default model", &current_model)
            .with_description("Saved default model for new sessions")
            .with_submenu(),
        SettingItem::new("thinking", "Default thinking", &current_thinking)
            .with_description("Saved default thinking level")
            .with_submenu(),
        SettingItem::new("scoped-models", "Cycle scope", &scoped_desc)
            .with_description("Models enabled for Ctrl+P cycling")
            .with_submenu(),
        SettingItem::new(
            "hide-thinking",
            "Hide thinking",
            bool_setting_value(hide_thinking),
        )
        .with_description("Hide thinking blocks in assistant responses")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "show-images",
            "Show images",
            bool_setting_value(show_images),
        )
        .with_description(if images_supported {
            "Render images inline in the terminal"
        } else {
            "Terminal cannot display inline images; previews stay as placeholders"
        })
        .with_values(&["true", "false"]),
        SettingItem::new(
            "cache-miss-notices",
            "Cache miss notices",
            bool_setting_value(show_cache_miss),
        )
        .with_description("Transcript notices for prompt-cache costs")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "quiet-startup",
            "Quiet startup",
            bool_setting_value(settings.quiet_startup.unwrap_or(false)),
        )
        .with_description("Disable the verbose startup listing (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "collapse-changelog",
            "Collapse changelog",
            bool_setting_value(settings.collapse_changelog.unwrap_or(false)),
        )
        .with_description("Show a compact upgrade notice (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "terminal-progress",
            "Terminal progress",
            bool_setting_value(settings.show_terminal_progress().unwrap_or(true)),
        )
        .with_description("OSC 9;4 progress in the terminal tab bar (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            bool_setting_value(settings.fullscreen_copy_on_select.unwrap_or(true)),
        )
        .with_description("Copy selected text automatically in fullscreen mode")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "double-escape-action",
            "Double-escape action",
            settings
                .double_escape_action
                .clone()
                .unwrap_or_else(|| "tree".to_string())
                .as_str(),
        )
        .with_description("Esc Esc with an empty editor (applies on restart)")
        .with_values(&["tree", "fork", "none"]),
        SettingItem::new(
            "editor-padding",
            "Editor padding",
            settings.editor_padding_x.unwrap_or(1).to_string().as_str(),
        )
        .with_description("Horizontal editor padding, 0-3 (applies on restart)")
        .with_values(&["0", "1", "2", "3"]),
        SettingItem::new(
            "autocomplete-max-items",
            "Autocomplete max items",
            settings
                .autocomplete_max_visible
                .unwrap_or(5)
                .to_string()
                .as_str(),
        )
        .with_description("Max autocomplete rows, 3-20 (applies on restart)")
        .with_values(&["3", "5", "7", "10", "15", "20"]),
        SettingItem::new("http-idle-timeout", "HTTP idle timeout", &http_idle)
            .with_description("Idle gap allowed while awaiting HTTP data (applies on restart)")
            .with_values(&["30s", "1m", "5m", "disabled"]),
    ]
}

/// `"true"` / `"false"` for a settings row value.
pub(super) fn bool_setting_value(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Note shown after the inline-image setting changes.
///
/// `protocol` is passed in rather than read from the terminal so the wording is
/// testable off a real terminal. Enabling images on a terminal that speaks
/// neither Kitty nor iTerm2 is a no-op the user cannot otherwise see: the image
/// component falls back to its `[Attached image]` placeholder, so a plain
/// "enabled" reads as a promise rpi does not keep.
pub(super) fn images_setting_note(enabled: bool, protocol: rpi_tui::ImageProtocol) -> String {
    match (enabled, protocol) {
        (false, _) => "Inline images disabled.".to_string(),
        (true, rpi_tui::ImageProtocol::None) => {
            "Inline images enabled, but this terminal cannot display them: ".to_string()
                + "images will stay as text placeholders. rpi renders inline images "
                + "through the Kitty graphics protocol (kitty, Ghostty, WezTerm, Warp) "
                + "or iTerm2 inline images."
        }
        (true, _) => "Inline images enabled.".to_string(),
    }
}

/// Apply one `/settings` value change to the running TUI and to `settings`.
///
/// Returns a note to show in the transcript when the change is user-visible;
/// `None` for changes that only take effect on the next launch.
pub(super) fn apply_setting_change(
    state: &Arc<TuiState>,
    key: &str,
    value: &str,
    settings: &mut crate::settings::Settings,
) -> Option<String> {
    let as_bool = || value == "true";
    match key {
        // Live: hide/show thinking blocks in the transcript immediately.
        "hide-thinking" => {
            state.set_hide_thinking(as_bool());
            settings.hide_thinking_block = Some(as_bool());
            Some(format!(
                "Thinking blocks {}.",
                if as_bool() { "hidden" } else { "shown" }
            ))
        }
        // Live: the image flag is consulted when images would be rendered.
        "show-images" => {
            state.set_show_images(as_bool());
            settings.show_images = Some(as_bool());
            Some(images_setting_note(
                as_bool(),
                rpi_tui::get_capabilities().protocol,
            ))
        }
        // Live: read from `state` once per cache-miss notice.
        "cache-miss-notices" => {
            if let Ok(mut guard) = state.cache_miss_notices.lock() {
                *guard = as_bool();
            }
            settings.show_cache_miss_notices = Some(as_bool());
            Some(format!(
                "Cache miss notices {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Live: read from settings on each selection mouse-up.
        "fullscreen-copy-on-select" => {
            settings.fullscreen_copy_on_select = Some(as_bool());
            Some(format!(
                "Copy on select {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Persisted only: resolved once at startup.
        "quiet-startup" => {
            settings.quiet_startup = Some(as_bool());
            None
        }
        "collapse-changelog" => {
            settings.collapse_changelog = Some(as_bool());
            None
        }
        "terminal-progress" => {
            settings.show_terminal_progress = Some(as_bool());
            None
        }
        "double-escape-action" => {
            settings.double_escape_action = Some(value.to_string());
            None
        }
        "editor-padding" => {
            settings.editor_padding_x = value.parse().ok();
            None
        }
        "autocomplete-max-items" => {
            settings.autocomplete_max_visible = value.parse().ok();
            None
        }
        "http-idle-timeout" => {
            settings.http_idle_timeout = Some(match value {
                "disabled" => serde_json::Value::String("disabled".to_string()),
                "30s" => serde_json::json!(30_000),
                "1m" => serde_json::json!(60_000),
                _ => serde_json::json!(300_000),
            });
            None
        }
        _ => None,
    }
}

/// Apply a theme choice AND persist it to settings.json (`/settings` → Theme).
pub(super) fn open_settings_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let mut items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    let registry = state
        .theme_registry
        .read()
        .ok()
        .map(|registry| registry.clone());
    if let Some(registry) = registry {
        for name in registry.names() {
            items.push(SelectItem::new(name, name));
        }
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    list.on_select(Arc::new(move |item| {
        let registry = state_sel.theme_registry.read().ok().map(|r| r.clone());
        match crate::themes::apply_and_persist(
            &item.value,
            &state_sel.theme_manager,
            registry.as_ref(),
        ) {
            Ok(message) => {
                state_sel.set_active_theme_spec(Some(item.value.clone()));
                if let Some(registry_now) = registry.as_ref() {
                    crate::themes::restart_theme_watcher(
                        &state_sel.theme_watcher,
                        Some(&item.value),
                        registry_now,
                        &state_sel.theme_manager,
                        theme_watch_notify(&state_sel, tui_sel.clone()),
                    );
                }
                add_note_message(&chat_sel, &message);
            }
            Err(message) => add_error_message(&chat_sel, &message),
        }
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        tui_sel.render_now(true);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// Choose the default model AND persist it (`/settings` → Default model):
/// applies live via `lane.set_model` and saves `defaultModel` to settings.json
/// (which `provider::resolve` honors as pi's `findInitialModel` step 3).
pub(super) fn open_settings_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) {
    let items = model_selector_items(catalog, lane_model_id);
    if items.is_empty() {
        add_note_message(&state.notice_container(), "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let catalog_arc = catalog.to_vec();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = find_model_selector_match(&catalog_arc, &item.value) else {
            add_note_message(&chat_sel, &format!("Model {} not found.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        let state = state_sel.clone();
        let tui = tui_sel.clone();
        tokio::spawn(async move {
            report_model_selection(&lane, &state, &tui, model, true).await;
        });
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Default model"),
        Some("Type to filter by name, provider, or id"),
        list,
        SelectorKind::Settings,
    );
}

/// Convert the authenticated runtime catalog into selector rows. Keep the
/// model id as the value so `/model <id>` and the selection callback share one
/// lookup path, while making the provider visible for OpenAI-compatible
/// gateways where the same model id may exist at multiple endpoints.
pub(super) fn model_selector_items(
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) -> Vec<SelectItem> {
    let mut seen = std::collections::HashSet::new();
    catalog
        .iter()
        .filter(|m| {
            seen.insert((
                m.api.clone(),
                m.provider.to_ascii_lowercase(),
                m.id.to_ascii_lowercase(),
            ))
        })
        .map(|m| {
            let label = if m.name.is_empty() {
                short_model_name(&m.id)
            } else {
                m.name.clone()
            };
            let identity = format!("{}/{}", m.provider, m.id);
            let marker = if m.id.eq_ignore_ascii_case(lane_model_id) {
                " (current)"
            } else {
                ""
            };
            // upstream ranks provider-prefixed queries first, so the bare id
            // is not the leading token (`getModelSelectorSearchText`).
            let name = if m.name.is_empty() {
                String::new()
            } else {
                format!(" {}", m.name)
            };
            let search_text = format!(
                "{} {}/{} {} {}{}",
                m.provider, m.provider, m.id, m.provider, m.id, name
            );
            // Value carries the provider-qualified identity so a bare model id
            // that exists at multiple endpoints can never resolve to the wrong
            // provider (`find_model_selector_match` understands both forms).
            SelectItem::new(&identity, &label)
                .with_description(&format!("{identity}{marker}"))
                .with_search_text(&search_text)
        })
        .collect()
}

/// Resolve a selector input by either bare model id or the qualified
/// `provider/model` identity shown for gateway models. This keeps manual
/// `/model ...` input consistent with the rows rendered by the selector.
/// A bare id that matches more than one provider is ambiguous and resolves to
/// `None` (never silently the first match).
pub(super) fn find_model_selector_match(
    catalog: &[rpi_ai::Model],
    input: &str,
) -> Option<rpi_ai::Model> {
    let (provider, id) = input
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .map_or((None, input), |(provider, id)| (Some(provider), id));
    let mut matches = catalog.iter().filter(|model| {
        model.id.eq_ignore_ascii_case(id)
            && provider.map_or(true, |provider| {
                model.provider.eq_ignore_ascii_case(provider)
                    || (provider.eq_ignore_ascii_case("anthropic")
                        && matches!(model.api, rpi_ai::Api::AnthropicMessages))
            })
    });
    match (matches.next(), matches.next()) {
        (Some(model), None) => Some(model.clone()),
        (None, None) if provider.is_some() => {
            let mut bare = catalog
                .iter()
                .filter(|model| model.id.eq_ignore_ascii_case(input));
            match (bare.next(), bare.next()) {
                (Some(model), None) => Some(model.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Read declarations from the live session so `/reload` updates `/provider` too.
pub(super) fn current_provider_manifests(
    session: &crate::session::ExtensionSessionCell,
) -> Vec<rpi_extensions::ProviderManifest> {
    let session = session.lock().unwrap_or_else(|error| error.into_inner());
    crate::provider::manifests_from_session(&session)
}

pub(super) fn current_oauth_providers(
    session: &crate::session::ExtensionSessionCell,
) -> Vec<rpi_extensions::OAuthProviderInfo> {
    let session = session.lock().unwrap_or_else(|error| error.into_inner());
    session
        .snapshot_arc()
        .map(|snapshot| rpi_extensions::list_oauth_providers(&snapshot))
        .unwrap_or_default()
}

/// Build `/provider` rows from the full catalog, including packages without
/// credentials. Label each with its model count, origin, protocol and key policy.
pub(super) fn provider_selector_items(
    manifests: &[rpi_extensions::ProviderManifest],
    current_provider: Option<&str>,
) -> Vec<SelectItem> {
    let catalog = crate::provider::catalog_with_manifests(manifests).unwrap_or_default();
    let models_cfg = crate::config::load_models_config().unwrap_or_default();
    let auth = crate::config::read_auth().unwrap_or_default();
    let auth_states = crate::credentials::statuses(&models_cfg, &auth, manifests);

    let mut providers: Vec<(String, usize)> = Vec::new();
    for model in &catalog {
        match providers.iter_mut().find(|(id, _)| *id == model.provider) {
            Some(entry) => entry.1 += 1,
            None => providers.push((model.provider.clone(), 1)),
        }
    }

    providers
        .into_iter()
        .map(|(id, count)| {
            let label = provider_display_name(&id);
            let marker = if current_provider.is_some_and(|current| id.eq_ignore_ascii_case(current))
            {
                " (current)"
            } else {
                ""
            };
            let manifest = manifests.iter().find(|manifest| manifest.id == id);
            let (origin, protocol) = provider_origin_protocol(&id, &models_cfg, manifest);
            let key_status = match auth_states.get(&id).map(|state| &state.source) {
                Some(crate::credentials::CredentialSource::Anonymous) => "无需 key",
                Some(crate::credentials::CredentialSource::Headers) => "已配置认证头",
                Some(crate::credentials::CredentialSource::Missing) | None => "未配置 key",
                _ => "已配置 key",
            };
            let description =
                format!("{count} model(s) · {origin} · {protocol} · {key_status}{marker}");
            SelectItem::new(&id, &label)
                .with_description(&description)
                .with_search_text(&format!("{label} {id} {origin} {protocol} {key_status}"))
        })
        .collect()
}

/// Build the provider rows shown by `/login`. Anonymous package providers are
/// intentionally omitted; built-in and configured providers remain available
/// even when they currently have no credential.
#[cfg(test)]
pub(super) fn login_provider_items(
    manifests: &[rpi_extensions::ProviderManifest],
) -> Vec<SelectItem> {
    login_provider_items_with_oauth(manifests, &[])
}

pub(super) fn login_provider_items_with_oauth(
    manifests: &[rpi_extensions::ProviderManifest],
    oauth_providers: &[rpi_extensions::OAuthProviderInfo],
) -> Vec<SelectItem> {
    let catalog = crate::provider::catalog_with_manifests(manifests).unwrap_or_default();
    let models_cfg = crate::config::load_models_config().unwrap_or_default();
    let auth = crate::config::read_auth().unwrap_or_default();
    let mut ids = Vec::new();
    for model in &catalog {
        if !ids.iter().any(|id: &String| id == &model.provider) {
            ids.push(model.provider.clone());
        }
    }
    for id in models_cfg.providers.keys() {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.clone());
        }
    }
    for manifest in manifests {
        if !ids.iter().any(|known| known == &manifest.id) {
            ids.push(manifest.id.clone());
        }
    }
    for oauth in oauth_providers {
        if !ids.iter().any(|known| known == &oauth.id) {
            ids.push(oauth.id.clone());
        }
    }

    ids.into_iter()
        .filter_map(|id| {
            let manifest = manifests.iter().find(|manifest| manifest.id == id);
            let oauth = oauth_providers.iter().find(|oauth| oauth.id == id);
            let configured = models_cfg.providers.get(&id);
            let requires_auth = oauth.is_some()
                || manifest.map_or_else(
                    || {
                        crate::credentials::builtin_api_key_env(&id).is_some()
                            || configured.is_some_and(|provider| {
                                provider.api_key.is_some() || provider.auth_header.is_some()
                            })
                    },
                    |manifest| manifest.requires_auth,
                );
            if !requires_auth {
                return None;
            }
            let credential = crate::credentials::resolve(&id, configured, &auth, manifest);
            let source = credential.source;
            let env = manifest
                .and_then(|manifest| manifest.api_key_env.as_deref())
                .or_else(|| crate::credentials::builtin_api_key_env(&id));
            let description = if oauth.is_some() {
                match source {
                    crate::credentials::CredentialSource::Missing => {
                        "未配置 · OAuth 登录".to_string()
                    }
                    crate::credentials::CredentialSource::StoredOauth => {
                        "已配置 · OAuth".to_string()
                    }
                    crate::credentials::CredentialSource::Environment(name) => {
                        format!("环境变量 {name} · 可重新登录")
                    }
                    _ => "已配置 · API key / OAuth".to_string(),
                }
            } else {
                let status = match source {
                    crate::credentials::CredentialSource::Missing => "未配置".to_string(),
                    crate::credentials::CredentialSource::Environment(name) => {
                        format!("环境变量 {name} · 可重新登录")
                    }
                    _ => "已配置".to_string(),
                };
                match env {
                    Some(env) => format!("{status} · API key / {env}"),
                    None => format!("{status} · API key"),
                }
            };
            Some(
                SelectItem::new(&id, &provider_display_name(&id))
                    .with_description(&description)
                    .with_search_text(&format!(
                        "{} {id} {description} login oauth",
                        provider_display_name(&id)
                    )),
            )
        })
        .collect()
}

/// Dispatch without holding the input-slot lock: submit and escape callbacks
/// clear or replace that same slot synchronously (API-key and OAuth login).
pub(super) fn handle_provider_login_key(state: &TuiState, key: KeyEvent) -> bool {
    let input = state.active_provider_input.lock().unwrap().clone();
    let Some(input) = input else {
        return false;
    };
    input.handle_key(key);
    true
}

/// Open the masked API-key input for one provider and save it under that
/// provider's own auth.json entry. A successful save asks the main loop to
/// reload the provider catalog so the newly authenticated models become live.
pub(super) fn begin_provider_login(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    extension_session: &crate::session::ExtensionSessionCell,
    provider_id: &str,
) {
    if current_oauth_providers(extension_session)
        .iter()
        .any(|provider| provider.id.eq_ignore_ascii_case(provider_id))
    {
        begin_provider_oauth_login(state, editor_container, editor, tui, tx, provider_id);
    } else {
        begin_api_key_login(state, editor_container, editor, tui, tx, provider_id);
    }
}

fn begin_api_key_login(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    provider_id: &str,
) {
    let provider_id = provider_id.to_string();
    let input = Arc::new(Input::with_placeholder("Paste API key, then press Enter"));
    input.set_masked(true);
    input.set_focused(true);
    let title = format!("Login · {provider_id}");
    let question = format!("API key for {provider_id}");
    let frame = provider_form_frame(
        &title,
        &question,
        input.clone(),
        "Enter saves to auth.json · Esc cancels",
    );
    editor.set_focused(false);
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    tui.set_focus(Some(input.clone()));

    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    let tx_submit = tx.clone();
    let provider_submit = provider_id.clone();
    input.on_submit(Arc::new(move |text| {
        let key = text.trim();
        if key.is_empty() {
            add_error_message(
                &state_submit.notice_container(),
                &format!("API key for {provider_submit} cannot be empty."),
            );
            tui_submit.request_render(false);
            return;
        }
        match crate::config::upsert_credential(
            &provider_submit,
            crate::config::Credential::ApiKey {
                key: Some(key.to_string()),
                env: None,
            },
        ) {
            Ok(()) => {
                add_note_message(
                    &state_submit.notice_container(),
                    &format!("Saved credentials for {provider_submit}; reloading providers…"),
                );
                let _ = tx_submit.send(TuiMessage::ReloadExtensions);
                finish_provider_login(&state_submit, &ec_submit, &editor_submit, &tui_submit);
            }
            Err(error) => add_error_message(
                &state_submit.notice_container(),
                &format!("Could not save {provider_submit} credentials: {error}"),
            ),
        }
        tui_submit.request_render(false);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    input.on_escape(Arc::new(move || {
        finish_provider_login(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        add_note_message(
            &state_cancel.notice_container(),
            "Provider login cancelled.",
        );
    }));
    tui.request_render(false);
}

pub(super) fn begin_provider_oauth_login(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    provider_id: &str,
) {
    let (flow_id, cancel) = state.begin_provider_auth();
    show_provider_auth_status(
        state,
        editor_container,
        editor,
        tui,
        &format!("Starting OAuth login for {provider_id}…"),
    );
    let _ = tx.send(TuiMessage::ProviderOAuthAction {
        provider: provider_id.to_string(),
        action: "begin".to_string(),
        input: serde_json::json!({}),
        purpose: ProviderOAuthPurpose::Login,
        flow_id,
        cancel,
    });
}
/// Start revoking a stored OAuth credential. The flow begins before its status
/// screen shows, so an Esc on that screen always ends this flow and cancels the
/// revoke it queued; an Esc before the screen shows sends no revoke at all.
#[allow(clippy::too_many_arguments)]
pub(super) fn begin_provider_oauth_logout(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    provider_id: &str,
    access: String,
    refresh: String,
) {
    let (flow_id, cancel) = state.begin_provider_auth();
    #[cfg(test)]
    if let Some(hook) = AFTER_PROVIDER_FLOW_BEGIN.with(std::cell::Cell::take) {
        hook();
    }
    show_provider_auth_status(
        state,
        editor_container,
        editor,
        tui,
        &format!("Revoking OAuth credentials for {provider_id}…"),
    );
    if !provider_flow_still_shown(state, editor_container, editor, tui, flow_id) {
        return;
    }
    let _ = tx.send(TuiMessage::ProviderOAuthAction {
        provider: provider_id.to_string(),
        action: "revoke".to_string(),
        input: serde_json::json!({
            "credential": {"access": access, "refresh": refresh}
        }),
        purpose: ProviderOAuthPurpose::Logout,
        flow_id,
        cancel,
    });
}
pub(super) fn show_provider_auth_status(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    message: &str,
) {
    let frame = provider_form_frame(
        "Provider authentication",
        message,
        Arc::new(Text::new("Please wait…", 0, 0)),
        "OAuth is running · wait for the provider response",
    );
    *state.active_provider_input.lock().unwrap() = None;
    editor.set_focused(false);
    editor_container.clear();
    editor_container.add_child(frame);
    tui.set_focus(None);
    tui.request_render(false);
}

pub(super) fn handle_provider_oauth_result(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    provider: &str,
    action: &str,
    result: Result<serde_json::Value, String>,
    purpose: ProviderOAuthPurpose,
    flow_id: u64,
) {
    // A result from a flow that already ended (Esc, or a newer flow) must not
    // act, so a login cancelled while its call ran can never save a credential.
    if state
        .provider_auth_flow_id
        .load(std::sync::atomic::Ordering::Acquire)
        != flow_id
    {
        return;
    }
    let response = result
        .map_err(|error| format!("OAuth {action} failed for {provider}: {error}"))
        .and_then(|value| {
            rpi_extensions::OAuthActionResponse::parse(value)
                .map_err(|error| format!("Invalid OAuth response from {provider}: {error}"))
        });
    // Every outcome but a login interaction ends the flow. Claim it before
    // acting, so a result that lost to Esc neither writes the store nor ends a
    // newer flow; after the claim only the editor is restored, since a flow
    // begun meanwhile is not this result's to end.
    let continues_flow = matches!(
        (purpose, &response),
        (ProviderOAuthPurpose::Login, Ok(response))
            if matches!(response.kind, rpi_extensions::OAuthResponseKind::Interaction)
    );
    if !continues_flow && !claim_provider_flow(state, flow_id) {
        return;
    }
    let response = match response {
        Ok(response) => response,
        Err(message) => {
            restore_provider_editor(state, editor_container, editor, tui);
            add_error_message(&state.notice_container(), &message);
            tui.request_render(false);
            return;
        }
    };

    match (purpose, response.kind) {
        (ProviderOAuthPurpose::Login, rpi_extensions::OAuthResponseKind::Interaction) => {
            begin_provider_oauth_input(
                state,
                editor_container,
                editor,
                tui,
                tx,
                provider,
                response.payload,
                flow_id,
            );
        }
        (ProviderOAuthPurpose::Login, rpi_extensions::OAuthResponseKind::Credential) => {
            match response.credential() {
                Ok(Some(token)) => match crate::config::upsert_oauth_token(
                    provider,
                    token.access,
                    token.refresh,
                    token.expires_at,
                ) {
                    Ok(()) => {
                        restore_provider_editor(state, editor_container, editor, tui);
                        add_note_message(
                            &state.notice_container(),
                            &format!(
                                "OAuth credentials saved for {provider}; reloading providers…"
                            ),
                        );
                        let _ = tx.send(TuiMessage::ReloadExtensions);
                    }
                    Err(error) => {
                        restore_provider_editor(state, editor_container, editor, tui);
                        add_error_message(
                            &state.notice_container(),
                            &format!("Could not save OAuth credentials for {provider}: {error}"),
                        );
                    }
                },
                Ok(None) | Err(_) => {
                    restore_provider_editor(state, editor_container, editor, tui);
                    add_error_message(
                        &state.notice_container(),
                        &format!("OAuth login for {provider} returned no valid credential."),
                    );
                }
            }
            tui.request_render(false);
        }
        (ProviderOAuthPurpose::Login, rpi_extensions::OAuthResponseKind::Error)
        | (ProviderOAuthPurpose::Login, rpi_extensions::OAuthResponseKind::Success) => {
            restore_provider_editor(state, editor_container, editor, tui);
            add_error_message(
                &state.notice_container(),
                &format!("OAuth login for {provider} did not return credentials."),
            );
            tui.request_render(false);
        }
        (ProviderOAuthPurpose::Logout, rpi_extensions::OAuthResponseKind::Success) => {
            restore_provider_editor(state, editor_container, editor, tui);
            match crate::config::delete_credential(provider) {
                Ok(true) => {
                    add_note_message(
                        &state.notice_container(),
                        &format!("OAuth credentials revoked for {provider}; reloading providers…"),
                    );
                    let _ = tx.send(TuiMessage::ReloadExtensions);
                }
                Ok(false) => add_note_message(
                    &state.notice_container(),
                    &format!("No saved credentials found for {provider}."),
                ),
                Err(error) => add_error_message(
                    &state.notice_container(),
                    &format!("Could not remove {provider} credentials: {error}"),
                ),
            }
            tui.request_render(false);
        }
        (ProviderOAuthPurpose::Logout, _) => {
            restore_provider_editor(state, editor_container, editor, tui);
            add_error_message(
                &state.notice_container(),
                &format!(
                    "OAuth revoke for {provider} did not succeed; local credentials were kept."
                ),
            );
            tui.request_render(false);
        }
    }
}

pub(super) fn begin_provider_oauth_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    provider: &str,
    payload: serde_json::Value,
    flow_id: u64,
) {
    let display = payload.get("display").unwrap_or(&payload);
    let title = display
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("OAuth authorization");
    let mut details = Vec::new();
    if let Some(url) = display.get("url").and_then(serde_json::Value::as_str) {
        details.push(format!("Open: {url}"));
    }
    if let Some(instructions) = display
        .get("instructions")
        .and_then(serde_json::Value::as_array)
    {
        details.extend(
            instructions
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string),
        );
    }
    if details.is_empty() {
        details.push("Enter the authorization value returned by the provider.".to_string());
    }
    let body = Arc::new(Container::new());
    body.add_child(Arc::new(Text::new(details.join("\n"), 0, 0)));
    body.add_child(Arc::new(Spacer::new(1)));
    let input = Arc::new(Input::with_placeholder("Paste OAuth code or response"));
    input.set_focused(true);
    body.add_child(input.clone());
    let frame = provider_form_frame(
        &format!("OAuth login · {provider}"),
        title,
        body,
        "Enter submits · Esc cancels this OAuth flow",
    );
    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    let tx_submit = tx.clone();
    let provider_submit = provider.to_string();
    input.on_submit(Arc::new(move |text| {
        let value = text.trim();
        if value.is_empty() {
            add_error_message(
                &state_submit.notice_container(),
                "OAuth input cannot be empty.",
            );
            tui_submit.request_render(false);
            return;
        }
        show_provider_auth_status(
            &state_submit,
            &ec_submit,
            &editor_submit,
            &tui_submit,
            &format!("Completing OAuth login for {provider_submit}…"),
        );
        add_note_message(
            &state_submit.notice_container(),
            &format!("Completing OAuth login for {provider_submit}…"),
        );
        let _ = tx_submit.send(TuiMessage::ProviderOAuthAction {
            provider: provider_submit.clone(),
            action: "exchange".to_string(),
            input: serde_json::json!({"value": value}),
            purpose: ProviderOAuthPurpose::Login,
            flow_id,
            cancel: state_submit.provider_auth_token(flow_id),
        });
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    input.on_escape(Arc::new(move || {
        if finish_provider_login(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel) {
            add_note_message(&state_cancel.notice_container(), "OAuth login cancelled.");
        }
    }));
    // Publish the form only once its callbacks exist, so no key reaches it
    // unhandled.
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    editor.set_focused(false);
    editor_container.clear();
    editor_container.add_child(frame);
    tui.set_focus(Some(input.clone()));
    if provider_flow_still_shown(state, editor_container, editor, tui, flow_id) {
        tui.request_render(false);
    }
}
#[cfg(test)]
thread_local! {
    /// Runs inside [`claim_provider_flow`] before the claim, so a test can
    /// land an Esc between a result's flow check and its credential write.
    pub(super) static BEFORE_PROVIDER_FLOW_CLAIM: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Runs in [`begin_provider_oauth_logout`] right after the flow begins, so
    /// a test can land an Esc before the status screen shows.
    pub(super) static AFTER_PROVIDER_FLOW_BEGIN: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// Call after a flow's screen or form is shown. An Esc handled on the key
/// worker while it was drawn already ended the flow and restored the editor,
/// which the drawing then covered: restore it again and report the flow over,
/// so no screen is left waiting for a result that never comes. A newer flow
/// that began meanwhile is left running; its own Esc and result still work.
fn provider_flow_still_shown(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    flow_id: u64,
) -> bool {
    match state
        .provider_auth_flow_id
        .load(std::sync::atomic::Ordering::Acquire)
    {
        active if active == flow_id => true,
        0 => {
            restore_provider_editor(state, editor_container, editor, tui);
            false
        }
        _ => false,
    }
}

/// Claim `flow_id` before a result ends it. Esc's
/// [`finish_provider_login`] swaps the same id to zero, so exactly one of a
/// cancel and a completion wins.
fn claim_provider_flow(state: &TuiState, flow_id: u64) -> bool {
    #[cfg(test)]
    if let Some(hook) = BEFORE_PROVIDER_FLOW_CLAIM.with(std::cell::Cell::take) {
        hook();
    }
    state.claim_provider_auth(flow_id)
}

/// End the provider auth UI and cancel its call. Returns whether this call
/// ended an active OAuth flow, i.e. won it against a completing result.
pub(super) fn finish_provider_login(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) -> bool {
    let ended = state.end_provider_auth();
    restore_provider_editor(state, editor_container, editor, tui);
    ended
}

/// Put the editor back in the input slot, without touching any flow.
fn restore_provider_editor(
    state: &TuiState,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    *state.active_provider_input.lock().unwrap() = None;
    editor_container.clear();
    editor_container.add_child(editor.clone());
    editor.set_focused(true);
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}
pub(super) fn stored_credential_items() -> Vec<SelectItem> {
    crate::config::read_auth()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, credential)| {
            let kind = match credential {
                crate::config::Credential::ApiKey { .. } => "API key",
                crate::config::Credential::Oauth { .. } => "OAuth",
            };
            SelectItem::new(&id, &provider_display_name(&id))
                .with_description(&format!("{kind} · remove saved credentials"))
                .with_search_text(&format!(
                    "{} {id} {kind} logout",
                    provider_display_name(&id)
                ))
        })
        .collect()
}
/// The providers that can be removed: everything defined in `models.json`
/// (built-ins are compiled in and cannot be deleted).
pub(super) fn removable_provider_items() -> Vec<SelectItem> {
    let cfg = crate::config::load_models_config().unwrap_or_default();
    cfg.providers
        .iter()
        .map(|(id, provider)| {
            let label = provider_display_name(id);
            SelectItem::new(id, &label)
                .with_description(&format!(
                    "{} model(s) · will be removed from models.json",
                    provider.models.len()
                ))
                .with_search_text(&format!("{label} {id} remove delete"))
        })
        .collect()
}

/// Where a provider comes from and what wire protocol it speaks, for the
/// `/provider` description column.
fn provider_origin_protocol(
    id: &str,
    cfg: &crate::config::ModelsConfig,
    manifest: Option<&rpi_extensions::ProviderManifest>,
) -> (&'static str, &'static str) {
    if let Some(manifest) = manifest {
        let api = cfg
            .providers
            .get(id)
            .and_then(|provider| provider.api.as_deref())
            .or_else(|| manifest.models.first().map(|model| model.api.as_str()));
        let protocol = match api {
            Some("anthropic-messages") => "Anthropic 兼容",
            Some("openai-completions" | "openai-responses") => "OpenAI 兼容",
            _ => "自定义协议",
        };
        return ("package", protocol);
    }
    match id.to_ascii_lowercase().as_str() {
        "anthropic" => ("内置", "Anthropic"),
        "openai" => ("内置", "OpenAI"),
        _ => {
            let protocol = cfg
                .providers
                .get(id)
                .and_then(|p| p.api.as_deref())
                .map(|api| match api {
                    "anthropic-messages" => "Anthropic 兼容",
                    "openai-completions" | "openai-responses" => "OpenAI 兼容",
                    _ => "自定义协议",
                })
                .unwrap_or("自定义协议");
            ("自定义", protocol)
        }
    }
}

/// Switch through the authenticated session catalog. Do not advertise success
/// or persist defaults until the live lane accepted the model.
pub(super) async fn switch_provider(
    input: &str,
    manifests: &[rpi_extensions::ProviderManifest],
    available: &[rpi_ai::Model],
    lane: &Arc<dyn AgentLane>,
) -> Result<(rpi_ai::Model, String), String> {
    let catalog =
        crate::provider::catalog_with_manifests(manifests).map_err(|error| error.to_string())?;
    let provider_id = canonicalize_provider_input(input, &catalog)?;
    let candidates: Vec<_> = available
        .iter()
        .filter(|model| model.provider == provider_id)
        .collect();
    if candidates.is_empty() {
        return Err(format!(
            "Provider \"{provider_id}\" has no authenticated model loaded in this session. Check rpi auth check --provider {provider_id}, then /reload and retry. The current provider is unchanged."
        ));
    }
    let mut settings = crate::settings::load_settings().map_err(|error| error.to_string())?;
    let config = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let configured = config.providers.get(&provider_id);
    let matches_api = |model: &&rpi_ai::Model| {
        configured
            .and_then(|config| config.api.as_deref())
            .map_or(true, |api| model.api.as_str() == api)
    };
    let current = lane.get_model().await.map_err(|error| error.to_string())?;
    let saved: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|model| {
            settings.default_provider.as_deref() == Some(provider_id.as_str())
                && settings
                    .default_model
                    .as_deref()
                    .is_some_and(|id| model.id.eq_ignore_ascii_case(id))
        })
        .collect();
    let model = candidates
        .iter()
        .copied()
        .find(|model| {
            model.provider == current.provider
                && model.api == current.api
                && model.id.eq_ignore_ascii_case(&current.id)
        })
        .or_else(|| saved.iter().copied().find(matches_api))
        .or_else(|| saved.first().copied())
        .or_else(|| {
            configured.and_then(|config| {
                config.models.first().and_then(|first| {
                    candidates
                        .iter()
                        .copied()
                        .filter(matches_api)
                        .find(|model| model.id.eq_ignore_ascii_case(&first.id))
                })
            })
        })
        .unwrap_or(candidates[0])
        .clone();
    lane.set_model(model.clone())
        .await
        .map_err(|error| error.to_string())?;
    settings.default_provider = Some(provider_id.clone());
    settings.default_model = Some(model.id.clone());
    let saved = match crate::settings::save_settings(&settings) {
        Ok(()) => "Default saved.".to_string(),
        Err(error) => format!("Changed for this session, but could not save the default: {error}"),
    };
    let note = format!(
        "Provider switched to {provider_id} (model {}) — applies to the next message. {saved}",
        model.id
    );
    Ok((model, note))
}

/// Persist a specific default provider + model pair after a live switch.
pub(super) fn set_default_model(provider_id: &str, model_id: &str) -> Result<String, String> {
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    settings.default_provider = Some(provider_id.to_string());
    settings.default_model = Some(model_id.to_string());
    crate::settings::save_settings(&settings).map_err(|error| error.to_string())?;
    Ok(format!(
        "Default provider saved as {provider_id} (model {model_id})."
    ))
}

// ===========================================================================
// Interactive "Add provider" form (`/provider` → Add new provider)
// ===========================================================================

/// Start the guided provider form. Each step renders its OWN single-line input
/// in the editor slot (isolated from the chat editor); Enter advances, Esc (or
/// an empty submission) cancels, and the final step writes `models.json`.
pub(super) fn begin_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    *state.provider_form.lock().unwrap() = Some(ProviderFormState::default());
    show_provider_form_step(state, editor_container, editor, tui);
}

/// Render the current form step in the editor slot. Choice-style steps (the
/// protocol step) render a select list; every other step renders a titled,
/// bordered single-line input.
fn show_provider_form_step(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let Some(form) = state.provider_form.lock().unwrap().clone() else {
        return;
    };
    *state.active_provider_input.lock().unwrap() = None;
    *state.active_provider_select.lock().unwrap() = None;

    if form.step == ProviderFormStep::Api {
        show_provider_api_selector(state, editor_container, editor, tui);
    } else {
        show_provider_text_input(state, editor_container, editor, tui, &form);
    }
}

/// Render a text-style form step as a titled single-line input.
fn show_provider_text_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ProviderFormState,
) {
    let (step_label, question, placeholder) = provider_step_prompt(&form.step);

    let input = Arc::new(Input::with_placeholder(placeholder));
    input.set_focused(true);

    let frame = provider_form_frame(
        step_label,
        question,
        input.clone(),
        "Enter next · Esc cancels (empty Enter also cancels)",
    );
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    tui.set_focus(Some(input.clone()));

    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    input.on_submit(Arc::new(move |text: &str| {
        handle_provider_form_input(&state_submit, &ec_submit, &editor_submit, &tui_submit, text);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    input.on_escape(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

/// Render the protocol step as a select list instead of a free-text input.
fn show_provider_api_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let items = vec![
        SelectItem::new("openai-completions", "OpenAI Completions")
            .with_description("OpenAI Chat Completions 兼容接口"),
        SelectItem::new("anthropic-messages", "Anthropic Messages")
            .with_description("Anthropic Messages 兼容接口"),
        SelectItem::new("openai-responses", "OpenAI Responses")
            .with_description("OpenAI Responses 接口"),
    ];
    let list = Arc::new(SelectList::new(items, 3));

    let frame = provider_form_frame(
        "Add provider · step 2/6",
        "Protocol — choose one with ↑/↓ and Enter",
        list.clone(),
        "Enter selects · Esc cancels",
    );
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_select.lock().unwrap() = Some(list.clone());
    tui.set_focus(Some(list.clone()));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        let Some(mut form) = state_sel.provider_form.lock().unwrap().take() else {
            return;
        };
        form.api = item.value.clone();
        form.step = ProviderFormStep::BaseUrl;
        *state_sel.provider_form.lock().unwrap() = Some(form);
        show_provider_form_step(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

/// Build the bordered frame shared by every provider-form step.
fn provider_form_frame(
    title: &str,
    question: &str,
    body: Arc<dyn Component>,
    hint: &str,
) -> Arc<Container> {
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(Text::new(title, 0, 0)));
    frame.add_child(Arc::new(Text::new(question, 0, 0)));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(body);
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(Text::new(hint, 0, 0)));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame
}

/// Prompt metadata for one form step: a heading, a question, and the input
/// placeholder.
fn provider_step_prompt(step: &ProviderFormStep) -> (&'static str, &'static str, &'static str) {
    match step {
        ProviderFormStep::Id => (
            "Add provider · step 1/6",
            "Provider id — a short unique name, e.g. openrouter.",
            "openrouter",
        ),
        ProviderFormStep::Api => (
            "Add provider · step 2/6",
            "Protocol — choose one below.",
            "",
        ),
        ProviderFormStep::BaseUrl => (
            "Add provider · step 3/6",
            "Base URL — OpenAI-compatible endpoints usually end in /v1.",
            "https://api.example.com/v1",
        ),
        ProviderFormStep::ApiKey => (
            "Add provider · step 4/6",
            "API key — a literal sk-… or an env reference like $SOME_API_KEY.",
            "$SOME_API_KEY",
        ),
        ProviderFormStep::ModelId => (
            "Add provider · step 5/6",
            "Model id — at least one.",
            "my-model",
        ),
        ProviderFormStep::ModelName => (
            "Add provider · step 6/6",
            "Model name — optional, empty keeps the id.",
            "My Model",
        ),
    }
}

/// Consume one input submission for the active form step, advancing to the
/// next step or writing `models.json` on the last one.
fn handle_provider_form_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    text: &str,
) {
    let Some(mut form) = state.provider_form.lock().unwrap().take() else {
        return;
    };
    let text = text.trim();

    // Empty submission cancels the form at any step.
    if text.is_empty() {
        cancel_provider_form(state, editor_container, editor, tui);
        return;
    }

    match form.step {
        ProviderFormStep::Id => {
            if provider_id_exists(text) {
                add_error_message(
                    &state.notice_container(),
                    &format!("Provider \"{text}\" already exists. Choose another id."),
                );
                *state.provider_form.lock().unwrap() = Some(form);
                show_provider_form_step(state, editor_container, editor, tui);
                return;
            }
            form.id = text.to_string();
            form.step = ProviderFormStep::Api;
        }
        ProviderFormStep::Api => {
            // The protocol step is rendered as a select list, so a text
            // submission can only arrive here via a bug; re-show the selector
            // rather than panicking.
            *state.provider_form.lock().unwrap() = Some(form);
            show_provider_form_step(state, editor_container, editor, tui);
            return;
        }
        ProviderFormStep::BaseUrl => {
            if !is_valid_provider_url(text) {
                add_error_message(
                    &state.notice_container(),
                    &format!("Invalid URL \"{text}\". It must start with http:// or https://."),
                );
                *state.provider_form.lock().unwrap() = Some(form);
                show_provider_form_step(state, editor_container, editor, tui);
                return;
            }
            form.base_url = text.to_string();
            form.step = ProviderFormStep::ApiKey;
        }
        ProviderFormStep::ApiKey => {
            form.api_key = text.to_string();
            form.step = ProviderFormStep::ModelId;
        }
        ProviderFormStep::ModelId => {
            form.model_id = text.to_string();
            form.step = ProviderFormStep::ModelName;
        }
        ProviderFormStep::ModelName => {
            form.model_name = text.to_string();
            match save_provider_from_form(&form) {
                Ok(note) => add_note_message(&state.notice_container(), &note),
                Err(error) => add_error_message(&state.notice_container(), &error),
            }
            finish_provider_form(state, editor_container, editor, tui);
            return;
        }
    }
    *state.provider_form.lock().unwrap() = Some(form);
    show_provider_form_step(state, editor_container, editor, tui);
}

/// Restore the chat editor and clear the form state (done or cancelled).
fn finish_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    *state.provider_form.lock().unwrap() = None;
    *state.active_provider_input.lock().unwrap() = None;
    *state.active_provider_select.lock().unwrap() = None;
    // The form never shares the chat editor, so clear it on exit: any draft or
    // stray paste must not resurface as if the user typed it.
    editor.clear();
    editor_container.clear();
    editor_container.add_child(editor.clone());
    editor.set_focused(true);
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}

fn cancel_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    add_note_message(&state.notice_container(), "Provider add cancelled.");
    finish_provider_form(state, editor_container, editor, tui);
}

/// Whether a provider id is already taken in `models.json`.
pub(super) fn provider_id_exists(id: &str) -> bool {
    crate::config::load_models_config()
        .map(|cfg| cfg.providers.contains_key(id))
        .unwrap_or(false)
}

/// A provider base URL must carry an http(s) scheme; anything else is almost
/// certainly a paste/copy mistake.
pub(super) fn is_valid_provider_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// Remove a configured provider from `models.json` (built-ins cannot be
/// removed). Clears a saved default that pointed at the removed provider.
pub(super) fn remove_provider(input: &str) -> Result<String, String> {
    let mut cfg = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let id = input.trim();
    if cfg.providers.shift_remove(id).is_none() {
        return Err(format!(
            "Provider \"{id}\" has no models.json configuration to remove. To remove an installed package, use rpi package unlink <name>."
        ));
    }
    crate::config::save_models_config(&cfg).map_err(|error| error.to_string())?;

    // Drop a saved default that referenced the removed provider so the next
    // launch falls back to auth-based selection instead of a stale id.
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    if settings.default_provider.as_deref() == Some(id) {
        settings.default_provider = None;
        settings.default_model = None;
        let _ = crate::settings::save_settings(&settings);
    }
    Ok(format!("Removed models.json configuration for provider \"{id}\" — restart rpi to apply. Installed provider packages remain available; use rpi package unlink to remove a package."))
}

/// Write the completed form into `models.json` as a new provider with one
/// model.
pub(super) fn save_provider_from_form(form: &ProviderFormState) -> Result<String, String> {
    crate::config::add_provider(
        &form.id,
        &form.api,
        &form.base_url,
        &form.api_key,
        &form.model_id,
        Some(&form.model_name),
        None,
        None,
        None,
    )?;
    Ok(format!(
        "Added provider \"{}\" ({} · {} model) — restart rpi to use it.",
        form.id, form.api, form.model_id
    ))
}

/// Human-friendly label for a provider id.
fn provider_display_name(id: &str) -> String {
    match id.to_ascii_lowercase().as_str() {
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "openai-completions" => "OpenAI Completions".to_string(),
        "openai-responses" => "OpenAI Responses".to_string(),
        _ => id.to_string(),
    }
}

/// Resolve a `/provider <id>` input against the catalog's provider ids: exact
/// match wins, then a unique case-insensitive match, mirroring the CLI
/// `--provider` canonicalization without weakening provider identity.
pub(super) fn canonicalize_provider_input(
    input: &str,
    catalog: &[rpi_ai::Model],
) -> Result<String, String> {
    let mut ids: Vec<&str> = Vec::new();
    for model in catalog {
        if !ids.iter().any(|id| *id == model.provider) {
            ids.push(&model.provider);
        }
    }
    if let Some(id) = ids.iter().find(|id| **id == input) {
        return Ok(id.to_string());
    }
    let matches: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| id.eq_ignore_ascii_case(input))
        .collect();
    match matches.as_slice() {
        [id] => Ok(id.to_string()),
        [] => Err(format!(
            "Unknown provider \"{input}\". Run /provider to browse available providers."
        )),
        _ => Err(format!(
            "Ambiguous provider \"{input}\": {}.",
            matches.join(", ")
        )),
    }
}

/// Choose the default thinking level AND persist it (`/settings` → Default
/// thinking): applies live via `lane.set_thinking_level` and saves
/// `defaultThinkingLevel` to settings.json.
pub(super) fn open_settings_thinking_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) {
    let model = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(lane_model_id));
    let levels: Vec<rpi_ai::types::ThinkingLevel> = model
        .map(|m| m.supported_thinking_levels())
        .unwrap_or_else(|| {
            use rpi_ai::types::ThinkingLevel::*;
            vec![Off, Minimal, Low, Medium, High]
        });
    let mut items: Vec<SelectItem> = Vec::new();
    for lvl in &levels {
        let name = thinking_level_name(*lvl);
        items.push(SelectItem::new(name, name).with_description(thinking_level_description(*lvl)));
    }
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "This model has no supported thinking levels.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(level) = thinking_level_from_name(&item.value) else {
            add_note_message(
                &chat_sel,
                &format!("Unknown thinking level: {}.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        let footer_sel = state_sel.footer.clone();
        tokio::spawn(async move {
            let _ = lane.set_thinking_level(level).await;
        });
        footer_sel.set_thinking_level(Some(thinking_level_name(level)));
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.default_thinking_level = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Default thinking set to {} (saved{}",
                item.label,
                if saved.is_ok() { ")" } else { ", not saved)" },
            ),
        );
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// `/scoped-models`: a multi-toggle selector over the catalog. Selecting an
/// item toggles it in the in-progress set (the selector stays open); Esc saves
/// the set to settings.json and closes. The active scoped set is echoed after
/// each toggle so the user sees the current selection.
pub(super) fn open_scoped_models_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
) {
    if catalog.is_empty() {
        add_note_message(&state.notice_container(), "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    // Seed the edit set from the saved scoped models.
    let seed: Vec<String> = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    *state.scoped_edit.lock().unwrap() = Some(seed);

    let mut items: Vec<SelectItem> = Vec::new();
    for m in catalog {
        items.push(SelectItem::new(&m.id, &m.id));
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let chat_sel = state.notice_container();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        // Toggle the model in the in-progress set; the selector stays open.
        let mut set = state_sel.scoped_edit.lock().unwrap();
        let set = set.get_or_insert_with(Vec::new);
        if let Some(pos) = set.iter().position(|m| m.eq_ignore_ascii_case(&item.value)) {
            set.remove(pos);
            add_note_message(&chat_sel, &format!("{} removed — Esc to save", item.label));
        } else {
            set.push(item.value.clone());
            add_note_message(&chat_sel, &format!("{} added — Esc to save", item.label));
        }
        tui_sel.request_render(false);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    let chat_cancel = state.notice_container();
    list.on_cancel(Arc::new(move || {
        // Save the edited set to settings.json and close.
        let set = state_cancel
            .scoped_edit
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.scoped_models = if set.is_empty() {
            None
        } else {
            Some(set.clone())
        };
        match crate::settings::save_settings(&settings) {
            Ok(()) => {
                if set.is_empty() {
                    add_note_message(&chat_cancel, "Ctrl+P cycles all models (scope cleared).");
                } else {
                    add_note_message(
                        &chat_cancel,
                        &format!("Ctrl+P cycle scope: {}", set.join(", ")),
                    );
                }
            }
            Err(e) => add_error_message(&chat_cancel, &format!("Could not save settings: {e}")),
        }
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::ScopedModels,
    );
}
