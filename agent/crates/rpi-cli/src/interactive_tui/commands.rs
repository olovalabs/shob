//! Built-in slash commands and extension command registration.

use super::*;

// ---- Built-in command implementations ----

pub(super) struct HelpCommand;
impl SlashCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "/help"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/?"]
    }
    fn description(&self) -> &'static str {
        "Show available commands"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_help_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct HistoryCommand;
impl SlashCommand for HistoryCommand {
    fn name(&self) -> &'static str {
        "/history"
    }
    fn description(&self) -> &'static str {
        "Browse history: /history [older|newer|latest] (default: older)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        if let Some(direction) = history::Direction::parse(args) {
            history::request(ctx.state.clone(), ctx.tui.clone(), direction);
        } else {
            add_error_message(
                &ctx.state.notice_container(),
                "Usage: /history [older|newer|latest]",
            );
            ctx.tui.request_render(false);
        }
    }
}

pub(super) struct ClearChatCommand;
impl SlashCommand for ClearChatCommand {
    fn name(&self) -> &'static str {
        "/clear"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/new"]
    }
    // `/new` carries its own weight as a discoverable entry, so surface it.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/new"]
    }
    fn description(&self) -> &'static str {
        "Start a new session (the previous one stays saved)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ClearChat);
    }
}

pub(super) struct ExitCommand;
impl SlashCommand for ExitCommand {
    fn name(&self) -> &'static str {
        "/exit"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/quit", "/q"]
    }
    // `/quit` is surfaced (matches pi's BUILTIN list); `/q` stays a hidden alias.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/quit"]
    }
    fn description(&self) -> &'static str {
        "Exit the application"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        ctx.state.cancel_js_preparation();
        let _ = ctx.tx.send(TuiMessage::Exit);
    }
}

pub(super) struct VersionCommand;
impl SlashCommand for VersionCommand {
    fn name(&self) -> &'static str {
        "/version"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/v"]
    }
    fn description(&self) -> &'static str {
        "Show version information"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_version_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct ChangelogCommand;
impl SlashCommand for ChangelogCommand {
    fn name(&self) -> &'static str {
        "/changelog"
    }
    fn description(&self) -> &'static str {
        "Show latest release changes (/changelog all for history)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        match args.trim() {
            "" => add_changelog_message(&ctx.chat),
            "all" => add_changelog_message_with_scope(&ctx.chat, true),
            _ => {
                ctx.chat
                    .add_child(Arc::new(Text::new("Usage: /changelog [all]", 1, 0)));
            }
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct HotkeysCommand;
impl SlashCommand for HotkeysCommand {
    fn name(&self) -> &'static str {
        "/hotkeys"
    }
    fn description(&self) -> &'static str {
        "Show keyboard shortcuts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_hotkeys_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct ModelCommand;
impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "/model"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/m"]
    }
    fn description(&self) -> &'static str {
        "Choose a model (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let term = args.trim();
        if !term.is_empty() {
            // /model <name> — direct switch by id (pi handleModelCommand).
            //
            // upstream only switches on an EXACT match; a term that matches
            // nothing falls through to the selector with the query prefilled
            // instead of erroring out (`showModelSelector(searchTerm)`).
            if let Some(model) = find_model_selector_match(
                &ctx.model_catalog
                    .read()
                    .unwrap_or_else(|error| error.into_inner()),
                term,
            ) {
                let lane = ctx.lane.clone();
                let state = ctx.state.clone();
                let tui = ctx.tui.clone();
                tokio::spawn(async move {
                    report_model_selection(&lane, &state, &tui, model, false).await;
                });
                return;
            }
        }
        let current_id = ctx.state.current_model_id();
        open_model_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.model_catalog
                .read()
                .unwrap_or_else(|error| error.into_inner()),
            &ctx.lane,
            &current_id,
            (!term.is_empty()).then_some(term),
        );
    }
}

pub(super) struct ProviderCommand;
impl SlashCommand for ProviderCommand {
    fn name(&self) -> &'static str {
        "/provider"
    }
    fn description(&self) -> &'static str {
        "Switch provider now, or add/remove provider configuration"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let term = args.trim();
        if term == "add" || term == "new" {
            begin_provider_form(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
            return;
        }
        if let Some(id) = term.strip_prefix("remove ") {
            match remove_provider(id) {
                Ok(note) => add_note_message(&ctx.state.notice_container(), &note),
                Err(error) => add_error_message(&ctx.state.notice_container(), &error),
            }
            ctx.tui.request_render(false);
            return;
        }
        if !term.is_empty() {
            let _ = ctx.tx.send(TuiMessage::SwitchProvider(term.to_string()));
            return;
        }
        let _ = ctx.tx.send(TuiMessage::OpenProviderSelector);
    }
}

pub(super) struct ThinkingCommand;
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &'static str {
        "/thinking"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/think"]
    }
    fn description(&self) -> &'static str {
        "Set thinking level (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let level_name = args.trim();
        if !level_name.is_empty() {
            // /thinking <level> — direct set (pi supports the param form).
            let Some(level) = thinking_level_from_name(level_name) else {
                add_error_message(
                    &ctx.state.notice_container(),
                    &format!(
                        "Unknown thinking level \"{level_name}\". Valid: {}",
                        crate::args::VALID_THINKING_LEVELS.join(", ")
                    ),
                );
                ctx.tui.request_render(false);
                return;
            };
            let lane = ctx.lane.clone();
            let footer = ctx.state.footer.clone();
            tokio::spawn(async move {
                let _ = lane.set_thinking_level(level).await;
            });
            footer.set_thinking_level(Some(thinking_level_name(level)));
            add_note_message(
                &ctx.state.notice_container(),
                &format!("Thinking set to {level_name}."),
            );
            ctx.tui.request_render(false);
            return;
        }
        let current_id = ctx.state.current_model_id();
        open_thinking_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog
                .read()
                .unwrap_or_else(|error| error.into_inner()),
            &current_id,
        );
    }
}

pub(super) struct ToolsCommand;
impl SlashCommand for ToolsCommand {
    fn name(&self) -> &'static str {
        "/tools"
    }
    fn description(&self) -> &'static str {
        "Toggle tools on/off"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_tools_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
        );
    }
}

pub(super) struct ImagesCommand;
impl SlashCommand for ImagesCommand {
    fn name(&self) -> &'static str {
        "/images"
    }
    fn description(&self) -> &'static str {
        "Toggle inline images"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_images_selector(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
    }
}

pub(super) struct SessionCommand;
impl SlashCommand for SessionCommand {
    fn name(&self) -> &'static str {
        "/session"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/resume"]
    }
    fn description(&self) -> &'static str {
        "List saved sessions"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_session_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.tx,
        );
    }
}

pub(super) struct ThemeCommand;
impl SlashCommand for ThemeCommand {
    fn name(&self) -> &'static str {
        "/theme"
    }
    fn description(&self) -> &'static str {
        "Choose a theme (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let arg = args.trim();
        if !arg.is_empty() {
            // /theme <name> — direct apply + persist (matches /settings Theme).
            // Resolution lives in `resolve_command_theme`: presets case-fold,
            // custom themes keep the original case (registry lookup is
            // case-sensitive, discovery preserves file stems).
            let registry = ctx
                .state
                .theme_registry
                .read()
                .ok()
                .map(|registry| registry.clone());
            let resolved = crate::themes::resolve_command_theme(arg, registry.as_ref());
            let Some(spec) = resolved else {
                add_error_message(
                    &ctx.state.notice_container(),
                    &format!(
                        "Unknown theme \"{arg}\". Valid: dark, light, monochrome, or a discovered theme."
                    ),
                );
                ctx.tui.request_render(false);
                return;
            };
            let registry = ctx
                .state
                .theme_registry
                .read()
                .ok()
                .map(|registry| registry.clone());
            match crate::themes::apply_and_persist(
                &spec,
                &ctx.state.theme_manager,
                registry.as_ref(),
            ) {
                Ok(message) => {
                    ctx.state.set_active_theme_spec(Some(spec.clone()));
                    let registry_now = ctx
                        .state
                        .theme_registry
                        .read()
                        .ok()
                        .map(|registry| registry.clone());
                    if let Some(registry_now) = registry_now.as_ref() {
                        crate::themes::restart_theme_watcher(
                            &ctx.state.theme_watcher,
                            Some(&spec),
                            registry_now,
                            &ctx.state.theme_manager,
                            theme_watch_notify(&ctx.state, ctx.tui.clone()),
                        );
                    }
                    add_note_message(&ctx.state.notice_container(), &message);
                }
                Err(message) => add_error_message(&ctx.state.notice_container(), &message),
            }
            ctx.tui.request_render(false);
            ctx.tui.render_now(true);
            return;
        }
        open_theme_selector(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
    }
}

pub(super) struct CompactCommand;
impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "/compact"
    }
    fn description(&self) -> &'static str {
        "Compact the conversation"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Compact);
    }
}

pub(super) struct CopyCommand;
impl SlashCommand for CopyCommand {
    fn name(&self) -> &'static str {
        "/copy"
    }
    fn description(&self) -> &'static str {
        "Copy last reply to clipboard"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Copy);
    }
}

pub(super) struct ExportCommand;
impl SlashCommand for ExportCommand {
    fn name(&self) -> &'static str {
        "/export"
    }
    fn description(&self) -> &'static str {
        "Export session (supports: md, html, jsonl)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let format = args.trim().to_lowercase();
        if format.is_empty() || format == "md" || format == "markdown" {
            let _ = ctx.tx.send(TuiMessage::ExportSession);
        } else if format == "html" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Html,
            ));
        } else if format == "jsonl" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Jsonl,
            ));
        } else {
            // Invalid format, show error
            add_error_message(
                &ctx.state.notice_container(),
                &format!(
                    "Unknown export format: {}. Supported: md, html, jsonl",
                    args.trim()
                ),
            );
            ctx.tui.request_render(false);
        }
    }
}

pub(super) struct ForkCommand;
impl SlashCommand for ForkCommand {
    fn name(&self) -> &'static str {
        "/fork"
    }
    fn description(&self) -> &'static str {
        "Fork the session into a new one"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

/// `/clone` is the upstream spelling for duplicating the current session.
/// Reuse the same durable fork path as `/fork`; both create a child session
/// and rebind the live harness to it.
pub(super) struct CloneCommand;
impl SlashCommand for CloneCommand {
    fn name(&self) -> &'static str {
        "/clone"
    }
    fn description(&self) -> &'static str {
        "Duplicate the current session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

pub(super) struct TreeCommand;
impl SlashCommand for TreeCommand {
    fn name(&self) -> &'static str {
        "/tree"
    }
    fn description(&self) -> &'static str {
        "Navigate the current session tree"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::OpenTree);
    }
}

pub(super) struct LoginCommand;
impl SlashCommand for LoginCommand {
    fn name(&self) -> &'static str {
        "/login"
    }
    fn description(&self) -> &'static str {
        "Log in to a provider (selector, API key, or OAuth)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            open_login_provider_selector(
                &ctx.state,
                &ctx.editor_container,
                &ctx.editor,
                &ctx.tui,
                &ctx.state.extension_session,
                &ctx.tx,
            );
            return;
        }

        let manifests = current_provider_manifests(&ctx.state.extension_session);
        let oauth_providers = current_oauth_providers(&ctx.state.extension_session);
        // Provider ids are taken from the catalog so case-insensitive input is
        // persisted under the canonical id. A single argument preserves the
        // historical `/login <key>` shortcut for Anthropic.
        let mut parts = args.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or_default();
        let login_items = login_provider_items_with_oauth(&manifests, &oauth_providers);
        let (provider, key) = match parts.next().map(str::trim).filter(|key| !key.is_empty()) {
            Some(key) => match login_items
                .iter()
                .find(|item| item.value.eq_ignore_ascii_case(first))
            {
                Some(item)
                    if oauth_providers
                        .iter()
                        .any(|oauth| oauth.id.eq_ignore_ascii_case(&item.value)) =>
                {
                    add_error_message(
                        &ctx.state.notice_container(),
                        &format!(
                            "Provider '{first}' uses OAuth. Use /login to start its interactive login."
                        ),
                    );
                    ctx.tui.request_render(false);
                    return;
                }
                Some(item) => (item.value.clone(), key.to_string()),
                None => {
                    add_error_message(
                        &ctx.state.notice_container(),
                        &format!(
                            "Unknown provider '{first}'. Use /login to choose a provider, or /login <api-key> for the Anthropic shortcut."
                        ),
                    );
                    ctx.tui.request_render(false);
                    return;
                }
            },
            None => {
                if login_items
                    .iter()
                    .any(|item| item.value.eq_ignore_ascii_case(first))
                {
                    add_error_message(
                        &ctx.state.notice_container(),
                        &format!("Missing API key for provider '{first}'."),
                    );
                    ctx.tui.request_render(false);
                    return;
                }
                ("anthropic".to_string(), args.to_string())
            }
        };
        match crate::config::upsert_credential(
            &provider,
            crate::config::Credential::ApiKey {
                key: Some(key.to_string()),
                env: None,
            },
        ) {
            Ok(()) => {
                add_note_message(
                    &ctx.state.notice_container(),
                    &format!("Saved credentials for {provider}; reloading providers…"),
                );
                let _ = ctx.tx.send(TuiMessage::ReloadExtensions);
            }
            Err(error) => add_error_message(
                &ctx.state.notice_container(),
                &format!("Could not save {provider} credentials: {error}"),
            ),
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct LogoutCommand;
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &'static str {
        "/logout"
    }
    fn description(&self) -> &'static str {
        "Remove saved provider credentials"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let provider = args.trim();
        if provider.is_empty() {
            open_logout_provider_selector(
                &ctx.state,
                &ctx.editor_container,
                &ctx.editor,
                &ctx.tui,
                &ctx.tx,
            );
            return;
        }
        let provider_id = crate::config::read_auth()
            .unwrap_or_default()
            .keys()
            .find(|id| id.eq_ignore_ascii_case(provider))
            .cloned()
            .unwrap_or_else(|| provider.to_string());
        let _ = ctx.tx.send(TuiMessage::LogoutProvider(provider_id));
        ctx.tui.request_render(false);
    }
}

pub(super) fn set_project_trust_for_command(
    cwd: &std::path::Path,
    value: Option<bool>,
) -> Result<(), crate::config::ConfigError> {
    crate::config::set_project_trust(cwd, value)
}

pub(super) struct TrustCommand;
impl SlashCommand for TrustCommand {
    fn name(&self) -> &'static str {
        "/trust"
    }
    fn description(&self) -> &'static str {
        "Trust the current project"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let value = match args.trim().to_ascii_lowercase().as_str() {
            "" | "yes" | "y" | "true" => Some(true),
            "no" | "n" | "false" => Some(false),
            "clear" | "reset" | "none" => None,
            _ => {
                add_note_message(
                    &ctx.state.notice_container(),
                    "Usage: /trust [yes|no|clear]",
                );
                ctx.tui.request_render(false);
                return;
            }
        };
        match set_project_trust_for_command(&ctx.cwd, value) {
            Ok(()) => {
                let label = match value {
                    Some(true) => "trusted",
                    Some(false) => "untrusted",
                    None => "trust decision cleared",
                };
                add_note_message(
                    &ctx.state.notice_container(),
                    &format!("Current project marked {label}."),
                );
            }
            Err(error) => add_error_message(
                &ctx.state.notice_container(),
                &format!("Could not save trust decision: {error}"),
            ),
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct NameCommand;
impl SlashCommand for NameCommand {
    fn name(&self) -> &'static str {
        "/name"
    }
    fn description(&self) -> &'static str {
        "Set session display name"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            add_note_message(
                &ctx.state.notice_container(),
                "Usage: /name <display name> — sets the current session's name.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::SetSessionName(name.to_string()));
    }
}

pub(super) struct ImportCommand;
impl SlashCommand for ImportCommand {
    fn name(&self) -> &'static str {
        "/import"
    }
    fn description(&self) -> &'static str {
        "Import a session file (path)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let path = args.trim();
        if path.is_empty() {
            add_note_message(
                &ctx.state.notice_container(),
                "Usage: /import <path-to-session.jsonl> — copies the file into the session dir and switches to it.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::ImportSession(path.to_string()));
    }
}

pub(super) struct SettingsCommand;
impl SlashCommand for SettingsCommand {
    fn name(&self) -> &'static str {
        "/settings"
    }
    fn description(&self) -> &'static str {
        "Open settings menu"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let current_id = ctx.state.current_model_id();
        open_settings_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog
                .read()
                .unwrap_or_else(|error| error.into_inner()),
            &current_id,
        );
    }
}

pub(super) struct ScopedModelsCommand;
impl SlashCommand for ScopedModelsCommand {
    fn name(&self) -> &'static str {
        "/scoped-models"
    }
    fn description(&self) -> &'static str {
        "Choose models for Ctrl+P cycling"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_scoped_models_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.model_catalog
                .read()
                .unwrap_or_else(|error| error.into_inner()),
        );
    }
}

pub(super) struct ShareCommand;
impl SlashCommand for ShareCommand {
    fn name(&self) -> &'static str {
        "/share"
    }
    fn description(&self) -> &'static str {
        "Share session (gist via gh, or clipboard)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShareSession);
    }
}

/// `/skills` lists the current skill registry, or explicitly invokes one.
/// Usage: `/skills` or `/skills <name> [additional instructions]`.
pub(super) struct SkillsCommand;
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &'static str {
        "/skills"
    }
    fn description(&self) -> &'static str {
        "List skills or invoke one with optional instructions"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let input = args.trim();
        if input.is_empty() {
            let resources = ctx.resources.lock().unwrap().clone();
            show_skills_panel(&ctx.chat, &resources);
            ctx.tui.request_render(false);
            return;
        }

        let mut parts = input.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let additional = parts.next().map(str::trim).filter(|text| !text.is_empty());
        let exists = ctx
            .resources
            .lock()
            .unwrap()
            .skills
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|skill| skill.name == name);
        if !exists {
            add_error_message(
                &ctx.state.notice_container(),
                &format!("Unknown skill \"{name}\". Use /skills to list available skills."),
            );
            ctx.tui.request_render(false);
            return;
        }
        if !ctx.state.try_start_working() {
            add_error_message(
                &ctx.state.notice_container(),
                "A run is already in progress. Press Esc to abort it first.",
            );
            ctx.tui.request_render(false);
            return;
        }

        add_skill_activation_message(&ctx.chat, name);
        let lane = ctx.lane.clone();
        let tui = ctx.tui.clone();
        let state = ctx.state.clone();
        let name = name.to_string();
        let additional = additional.map(str::to_string);
        let streaming = ctx.streaming;
        tokio::spawn(async move {
            run_skill_streaming(&lane, &name, additional.as_deref(), &tui, &state, streaming).await;
        });
    }
}

/// `/context` — browse loaded resources in a dismissible, read-only selector.
pub(super) struct ContextCommand;
impl SlashCommand for ContextCommand {
    fn name(&self) -> &'static str {
        "/context"
    }
    fn description(&self) -> &'static str {
        "Browse loaded context files, skills, and prompts (Esc to close)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let resources = ctx.resources.lock().unwrap().clone();
        open_context_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &resources,
        );
    }
}

/// `/usage` — token/cost/cache totals read through the product-layer
/// `AgentSession`. This is the same numbers the footer shows, but expanded and
/// with the cache-hit breakdown that the one-line footer cannot fit.
pub(super) struct UsageCommand;
impl SlashCommand for UsageCommand {
    fn name(&self) -> &'static str {
        "/usage"
    }
    fn description(&self) -> &'static str {
        "Show token, cost, and cache totals for this session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShowUsage);
        ctx.tui.request_render(false);
    }
}

/// `/reload` — re-run extension + resource discovery into the LIVE harness
/// (B5d): reload the cdylib plugins, invalidate the old `ActionBridge` +
/// registry snapshot, rebuild skills/prompts/context/SYSTEM.md/APPEND_SYSTEM.md
/// + the `TeeEmitter`, and push the rebuilt state via the B5d harness setters.
/// The command itself runs on the blocking submit thread, so it can't drive
/// the async `reload_extension_resources` routine directly — it signals the main
/// loop via `TuiMessage::ReloadExtensions`, which awaits it on the async runtime.
/// (A plugin's `runtime_action(Reload)` signals the same loop via the
/// `ReloadMailbox` the TUI installs — the B5d async-reload design avoids the
/// self-unmapping race a synchronous plugin-initiated reload would have.)
pub(super) struct ReloadCommand;
impl SlashCommand for ReloadCommand {
    fn name(&self) -> &'static str {
        "/reload"
    }
    fn description(&self) -> &'static str {
        "Reload extensions, skills, prompts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        // Signal the main loop. It owns the `&AgentHarness` borrow the
        // `reload_extension_resources` routine needs (the blocking submit thread
        // only has the context's `Arc<ReloadContext>` + the `Arc<dyn AgentLane>`).
        add_note_message(
            &ctx.state.notice_container(),
            "Reloading extensions + resources…",
        );
        ctx.tui.request_render(false);
        let _ = ctx.tx.send(TuiMessage::ReloadExtensions);
    }
}

/// Build the full command registry: active built-ins first (so they win on a
/// fuzzy autocomplete tie), then the v1-out-of-scope stubs. Prompt-template
/// commands are merged in separately by the autocomplete builder (they dispatch
/// via template expansion, not this registry).
pub(super) fn build_builtin_registry() -> CommandRegistry {
    let mut r = CommandRegistry::new();
    r.register(Arc::new(HelpCommand));
    r.register(Arc::new(ClearChatCommand));
    r.register(Arc::new(HistoryCommand));
    r.register(Arc::new(ExitCommand));
    r.register(Arc::new(VersionCommand));
    r.register(Arc::new(ChangelogCommand));
    r.register(Arc::new(ModelCommand));
    r.register(Arc::new(ProviderCommand));
    r.register(Arc::new(ThinkingCommand));
    r.register(Arc::new(ToolsCommand));
    r.register(Arc::new(ImagesCommand));
    r.register(Arc::new(SessionCommand));
    r.register(Arc::new(ThemeCommand));
    r.register(Arc::new(CompactCommand));
    r.register(Arc::new(CopyCommand));
    r.register(Arc::new(HotkeysCommand));
    r.register(Arc::new(ContextCommand));
    r.register(Arc::new(SkillsCommand));
    r.register(Arc::new(UsageCommand));
    // Recognized but inert in v1 (one struct backs them all). The TS builtins
    // out of v1 scope; each carries a description so autocomplete surfaces its
    // existence even though running it reports "not supported".
    r.register(Arc::new(NameCommand));
    r.register(Arc::new(SettingsCommand));
    r.register(Arc::new(ScopedModelsCommand));
    r.register(Arc::new(ExportCommand));
    r.register(Arc::new(ImportCommand));
    r.register(Arc::new(ShareCommand));
    r.register(Arc::new(ForkCommand));
    r.register(Arc::new(CloneCommand));
    r.register(Arc::new(TreeCommand));
    r.register(Arc::new(TrustCommand));
    r.register(Arc::new(LoginCommand));
    r.register(Arc::new(LogoutCommand));
    r.register(Arc::new(ReloadCommand));
    r
}

pub(super) fn register_extension_commands(
    registry: &mut CommandRegistry,
    session: crate::session::ExtensionSessionCell,
) {
    let commands = session
        .lock()
        .ok()
        .and_then(|s| s.snapshot_arc())
        .map(|snap| snap.commands().to_vec())
        .unwrap_or_default();
    for command in commands {
        let name = if command.name.starts_with('/') {
            command.name.clone()
        } else {
            format!("/{}", command.name)
        };
        if registry.find(&name).is_some() {
            continue;
        }
        registry.register(Arc::new(ExtensionCommand {
            name,
            description: command.description,
            session: session.clone(),
        }));
    }
}
