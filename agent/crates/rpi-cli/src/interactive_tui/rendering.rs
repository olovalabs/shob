//! Transcript messages, welcome panels, errors, and session resume hints.

use super::*;

// ===========================================================================
// Transcript message helpers
// ===========================================================================

/// Add a welcome header with no capability snapshot.
///
/// Kept for tests and for callers that genuinely have nothing to advertise;
/// production paths that rebuild the transcript must use
/// [`add_welcome_message_with_capabilities`] with [`current_capabilities`]
/// instead, otherwise the banner renders `Tools (0) none` / `Skills (0) none`
/// for a session that has both.
#[cfg(test)]
pub(super) fn add_welcome_message(container: &Arc<Container>) {
    add_welcome_message_with_capabilities(container, &[], &[], &[]);
}

/// Snapshot the capabilities the welcome header advertises: active tool names,
/// discovered skills, and loaded context files.
///
/// Every path that rebuilds the transcript for a different session (`/new`,
/// `/session`, `/import`, `/fork`, tree navigation) must use this rather than
/// [`add_welcome_message`], which passes empty slices and therefore rendered
/// `Tools (0) none` / `Skills (0) none` even though the session had both.
pub(super) async fn current_capabilities(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let resources = harness.get_resources().await.unwrap_or_default();
    capabilities_from_resources(harness, lane, &resources).await
}

/// [`current_capabilities`] against an already-fetched resource snapshot, so a
/// caller that needs the same snapshot for another purpose (the startup
/// prompt-template registry) does not trigger a second resource scan.
pub(super) async fn capabilities_from_resources(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    resources: &rpi_harness::types::AgentHarnessResources,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut active_tools = lane.get_active_tools().await.unwrap_or_default();
    // AgentHarness uses an empty active-name list as the default "all tools"
    // state. Do not expose that implementation sentinel as `Tools (0) none`
    // in the welcome banner.
    if active_tools.is_empty() {
        active_tools = harness
            .get_tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
    }
    let skills: Vec<String> = resources
        .skills
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    (active_tools, skills, resources.context_files.clone())
}

/// Add the startup welcome header and a compact snapshot of active tools and
/// discovered skills and context files. The snapshot reflects the first turn's
/// configuration, including tools contributed by extensions.
pub(super) fn add_welcome_message_with_capabilities(
    container: &Arc<Container>,
    active_tools: &[String],
    skills: &[String],
    context_files: &[String],
) -> Arc<Container> {
    let c = current_theme().colors;
    // The mark, wordmark and language mark live in `crate::brand`. See that
    // module for why the layout is built from single-width characters only.
    container.add_child(Arc::new(crate::brand::BrandLockup::new()));
    container.add_child(Arc::new(Spacer::new(1)));
    // The mark carries the product name, so the line under it is the tagline
    // rather than a second logotype.
    let title = c.muted.fg("interactive TUI · library-first agent runtime");
    container.add_child(Arc::new(Text::new(title, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        c.dim.fg("Type your message and press Enter to send."),
        1,
        0,
    )));
    let hint = c
        .dim
        .fg("Enter send · Shift+Enter newline · Ctrl+C clear · Esc abort · /help");
    container.add_child(Arc::new(Text::new(hint, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    let context_listing = Arc::new(Container::new());
    render_context_files(&context_listing, context_files);
    container.add_child(context_listing.clone());
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Tools", active_tools),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Skills", skills),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::new()));
    context_listing
}

/// Retain this section in the welcome header so /reload can replace it.
pub(super) fn render_context_files(container: &Arc<Container>, files: &[String]) {
    container.clear();
    if files.is_empty() {
        return;
    }
    let colors = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        colors.accent.fg(&format!("Context ({})", files.len())),
        1,
        0,
    )));
    for path in files {
        container.add_child(Arc::new(Text::new(
            colors.dim.fg(&format!("  {path}")),
            1,
            0,
        )));
    }
}

pub(super) fn update_resource_listing(
    shared: &Arc<Mutex<rpi_harness::types::AgentHarnessResources>>,
    startup_context: Option<&Arc<Container>>,
    resources: rpi_harness::types::AgentHarnessResources,
) {
    if let Some(listing) = startup_context {
        render_context_files(listing, &resources.context_files);
    }
    *shared.lock().unwrap() = resources;
}

/// Append update notices inside the live transcript. Fullscreen mode clears
/// pre-TUI stdout/stderr, so update state must be represented by components.
pub(super) fn add_update_notices(
    container: &Arc<Container>,
    report: &crate::updates::UpdateReport,
) {
    let colors = current_theme().colors;
    let group = Arc::new(Container::new());

    for warning in &report.warnings {
        let body = format!(
            "{}\n{} {}{}",
            colors.error.fg(&warning.message),
            colors.muted.fg("Run"),
            colors.accent.fg(&warning.command),
            colors.muted.fg(" to retry.")
        );
        add_update_panel(&group, "Update Failed", &body, colors.error);
    }

    if let Some(notice) = report.notices.iter().find(|notice| notice.name == "rpi") {
        let body = format!(
            "{} {}{}",
            colors
                .muted
                .fg(&format!("New version {} is available. Run", notice.latest)),
            colors.accent.fg(&notice.command),
            colors.muted.fg(".")
        );
        add_update_panel(&group, "Update Available", &body, colors.warning);
    }

    for notice in report.notices.iter().filter(|notice| notice.name != "rpi") {
        let body = format!(
            "{}\n{} {}",
            colors.muted.fg(&format!(
                "{} {} -> {}",
                notice.name, notice.current, notice.latest
            )),
            colors.muted.fg("Run"),
            colors.accent.fg(&notice.command)
        );
        add_update_panel(&group, "Package Update Available", &body, colors.warning);
    }

    // Other transcript producers append concurrently. Add the fully built
    // group in one operation so card borders and content cannot interleave
    // with user, assistant, tool, or extension messages.
    if group.child_count() > 0 {
        container.add_child(group);
    }
}

pub(super) fn add_update_panel(
    container: &Arc<Container>,
    title: &str,
    body: &str,
    color: rpi_tui::Color,
) {
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
    container.add_child(Arc::new(Text::new(
        format!("{}\n{body}", color.fg(&tui_bold(title))),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
}

pub(super) fn welcome_capability_line(label: &str, names: &[String]) -> String {
    let c = current_theme().colors;
    let value = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(" · ")
    };
    format!(
        "{} {}",
        c.accent.fg(&format!("{label} ({})", names.len())),
        c.muted.fg(&value)
    )
}

/// Add the `/help` command listing to the chat container.
pub(super) fn add_help_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    // Section header + a thin themed rule, then a two-column command table:
    // `cmd` in accent, `— desc` in muted. The old single-space layout made
    // the description column wander depending on command length.
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📚 Available Commands")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));

    let cmds: &[(&str, &str)] = &[
        ("/help, /?", "Show this help message"),
        ("/clear, /new", "Clear the conversation"),
        ("/exit, /quit, /q", "Exit the application"),
        ("/version, /v", "Show version information"),
        ("/changelog", "Show recent release changes"),
        (
            "/history",
            "Browse older/newer/latest history (default: older)",
        ),
        ("/model, /m", "Choose a model (live switch)"),
        (
            "/provider",
            "Switch provider now, or add/remove provider configuration",
        ),
        ("/thinking, /think", "Set reasoning depth (selector)"),
        ("/tools", "Toggle built-in tools on/off"),
        ("/images", "Toggle inline image rendering"),
        ("/session", "List saved sessions"),
        ("/theme", "Choose a theme (selector)"),
        ("/context", "Browse loaded resources (Esc to close)"),
        ("/compact", "Compact the conversation"),
        ("/copy", "Copy last reply to clipboard"),
        ("/hotkeys", "Show keyboard shortcuts"),
        ("/skill:<name>", "Invoke a skill with optional instructions"),
        (
            "/skills",
            "List skills (legacy invocation: /skills <name> [instructions])",
        ),
    ];
    let cmd_w = cmds.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (cmd, desc) in cmds {
        let row = format!(
            "  {:<cmd_w$}  {}  {}",
            c.accent.fg(cmd),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/version` block to the chat container.
pub(super) fn add_version_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📦 Version Information")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    // Use the crate version (kept in sync via `version.workspace = true`)
    // instead of the stale hardcoded "v0.1.2".
    container.add_child(Arc::new(Text::new(
        format!(
            "  {} {}",
            c.muted.fg("rpi-cli"),
            c.text.fg(&format!("v{}", crate::VERSION))
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        format!(
            "  {}",
            c.dim.fg("Rust implementation of pi coding agent TUI")
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Show the real bundled changelog, including in installed/offline builds.
pub(super) fn add_changelog_message(container: &Arc<Container>) {
    add_changelog_message_with_scope(container, false);
}

pub(super) fn add_changelog_message_with_scope(container: &Arc<Container>, all: bool) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold(if all {
            "Full Changelog"
        } else {
            "Recent Changes"
        })),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let entries = crate::changelog::bundled_entries();
    let entries = if all {
        entries
    } else {
        crate::changelog::latest_release(&entries, crate::VERSION)
    };
    let markdown = crate::changelog::format_entries(&entries);
    let markdown = if markdown.is_empty() {
        "No changelog entries found.".into()
    } else {
        markdown
    };
    container.add_child(Arc::new(Markdown::new(markdown, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

pub(super) fn add_startup_changelog_message(
    container: &Arc<Container>,
    markdown: &str,
    version: &str,
    collapse: bool,
) {
    container.add_child(Arc::new(DynamicBorder::new()));
    if collapse {
        container.add_child(Arc::new(Text::new(
            format!("Updated to v{version}. Use /changelog to view release notes."),
            1,
            0,
        )));
    } else {
        let c = current_theme().colors;
        container.add_child(Arc::new(Text::new(
            c.md_heading.fg(&tui_bold("What's New")),
            1,
            0,
        )));
        container.add_child(Arc::new(Spacer::new(1)));
        container.add_child(Arc::new(Markdown::new(markdown.to_owned(), 1, 0)));
        container.add_child(Arc::new(Spacer::new(1)));
    }
    container.add_child(Arc::new(DynamicBorder::new()));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/hotkeys` block to the chat container.
pub(super) fn add_hotkeys_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("⌨️  Keyboard Shortcuts")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let keys: &[(&str, &str)] = &[
        ("Enter", "Send message"),
        ("Shift+Enter", "New line"),
        ("Tab", "Accept autocomplete suggestion"),
        ("Ctrl+A / Ctrl+E", "Line start / end"),
        (
            "Ctrl+K / Ctrl+U",
            "Kill to end / start of line (Ctrl+Y yanks)",
        ),
        ("Ctrl+- / Ctrl+R", "Undo / redo"),
        ("Ctrl+Y / Alt+Y", "Yank / yank-pop"),
        ("Alt+Backspace", "Kill previous word"),
        ("Ctrl+C", "Clear the editor (press twice quickly to exit)"),
        (
            "Ctrl+X",
            "Copy the mouse/editor selection or the last reply",
        ),
        ("Esc", "Abort a running prompt"),
        ("Ctrl+L", "Open model selector"),
        ("Ctrl+P", "Cycle to the next model (live)"),
        ("Ctrl+O", "Expand/collapse all tool output"),
        ("Ctrl+T", "Show/hide reasoning blocks"),
        ("PageUp/Down", "Scroll transcript by one page"),
        ("Home / End", "Jump to transcript start / latest output"),
    ];
    let key_w = keys.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, desc) in keys {
        let row = format!(
            "  {:<key_w$}  {}  {}",
            c.accent.fg(key),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a user message echo to the chat container — a bordered `UserMessageComponent`
/// (surface-colored box with OSC133 prompt-boundary markers) replacing the old
/// plain `> text` echo. A trailing Spacer(1) separates it from the next
// transcript entry (every entry contributes one trailing spacer so
// consecutive turns are separated by exactly one blank line).
pub(super) fn add_user_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(UserMessageComponent::new(text.to_string())));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add an error message to the chat container.
/// Collect all rendered lines from the chat container for search.
pub(super) fn collect_transcript_lines(chat_container: &Arc<Container>) -> Vec<String> {
    let width = 80; // Default width for search; actual width varies by terminal
    chat_container.render(width)
}

/// Build the shell command that resumes `session_id` in this project, or
/// `None` when there is nothing to resume. Mirrors upstream's
/// `formatResumeCommand`: `rpi --session <id>`, prefixing
/// `--session-dir <dir>` only when a non-default directory was requested.
pub(super) fn format_resume_command(
    session_id: &str,
    cwd: &std::path::Path,
    session_dir: Option<&std::path::Path>,
) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut args = vec![crate::APP_NAME.to_string()];
    if let Some(dir) = session_dir {
        let default = crate::session::default_session_dir(cwd);
        if dir != default.as_path() {
            args.push("--session-dir".to_string());
            args.push(quote_shell_arg(&dir.to_string_lossy()));
        }
    }
    args.push("--session".to_string());
    args.push(session_id.to_string());
    Some(args.join(" "))
}

/// Quote a shell argument when it contains whitespace or shell metacharacters
/// (mirrors upstream's `quoteIfNeeded`).
pub(super) fn quote_shell_arg(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || "\"'`$&|;<>()*?[]{}!~#\\".contains(c));
    if needs_quoting {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

pub(super) fn add_error_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    let text = sanitize_error_message(text);
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.error.fg("✗"), c.error.fg(&text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Keep provider diagnostics printable in the transcript. HTTP error bodies
/// may contain carriage returns, terminal escapes, or an unexpectedly large
/// JSON payload; letting those bytes reach the renderer can corrupt the input
/// row or make the whole frame exceed terminal limits.
pub(super) fn sanitize_error_message(text: &str) -> String {
    const MAX_ERROR_CHARS: usize = 16 * 1024;
    let mut result = String::with_capacity(text.len().min(MAX_ERROR_CHARS));
    let mut count = 0;
    // 0 = normal, 1 = escape introducer, 2 = CSI, 3 = OSC.
    let mut escape_mode = 0u8;
    for ch in text.chars() {
        if escape_mode != 0 {
            match escape_mode {
                1 if ch == '[' => escape_mode = 2,
                1 if ch == ']' => escape_mode = 3,
                1 if ch == '\x07' || ('@'..='~').contains(&ch) => escape_mode = 0,
                2 if ('@'..='~').contains(&ch) => escape_mode = 0,
                3 if ch == '\x07' => escape_mode = 0,
                _ => {}
            }
            continue;
        }
        if ch == '\x1b' {
            escape_mode = 1;
            continue;
        }
        if count >= MAX_ERROR_CHARS {
            result.push('…');
            break;
        }
        match ch {
            '\n' | '\t' => {
                result.push(ch);
                count += 1;
            }
            '\r' => {}
            c if c.is_control() => {}
            c => {
                result.push(c);
                count += 1;
            }
        }
    }
    if result.trim().is_empty() {
        "Provider request failed.".to_string()
    } else {
        result
    }
}

/// Add a neutral note (e.g. unsupported-command message) to the chat container.
pub(super) fn add_note_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.info.fg("ℹ"), c.muted.fg(text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Build the theme-watcher notify closure: append the reload notice to the
/// transcript and force a full content repaint. Appending alone never requests
/// a frame and the scheduler only paints idle sessions on size/status changes,
/// so without the repaint a background reload (or its parse error) stays
/// invisible until the next interaction repaints the screen.
pub(super) fn theme_watch_notify(
    state: &Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) -> impl Fn(Result<String, String>) + Send + Sync + 'static {
    // The watcher lives in TuiState; a weak reference avoids a cycle and lets
    // each callback resolve the visible page after history/session navigation.
    let state = Arc::downgrade(state);
    move |result| {
        let Some(state) = state.upgrade() else {
            return;
        };
        let chat = state.notice_container();
        match result {
            Ok(message) => add_note_message(&chat, &message),
            Err(message) => add_error_message(&chat, &message),
        }
        // A full repaint (not scroll-cache reuse) so already-styled
        // transcript rows pick up the new palette too.
        tui.request_render(true);
    }
}

/// Add an explicit skill activation notice to the transcript.
pub(super) fn add_skill_activation_message(container: &Arc<Container>, name: &str) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        format!(
            "  {} {}",
            c.accent.fg("[skill]"),
            c.text.fg(&format!("Activating {name}")),
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// List the skills currently available to this session.
pub(super) fn show_skills_panel(
    chat: &Arc<Container>,
    resources: &rpi_harness::types::AgentHarnessResources,
) {
    let skills = resources.skills.as_deref().unwrap_or(&[]);
    let mut lines = Vec::new();
    lines.push("🧩 Skills available in this session:".to_string());
    if skills.is_empty() {
        lines.push("  (none discovered — create .rpi/skills/ or ~/.rpi/agent/skills/)".into());
    } else {
        for skill in skills {
            let hidden = if skill.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            lines.push(format!(
                "  • {}{} — {}",
                skill.name,
                hidden,
                skill.description.chars().take(72).collect::<String>()
            ));
        }
        lines.push("  Invoke with: /skill:<name> [instructions]".into());
        lines.push("  Compatibility: /skills <name> [instructions]".into());
    }
    container_note_block(chat, &lines.join("\n"));
}

/// Build read-only `/context` rows from the resource snapshot, initialized at
/// startup and refreshed on reload. The selector owns these rows, not the chat.
pub(super) fn context_panel_lines(
    resources: &rpi_harness::types::AgentHarnessResources,
) -> Vec<String> {
    let skills = resources.skills.as_deref().unwrap_or(&[]);
    let templates = resources.prompt_templates.as_deref().unwrap_or(&[]);
    let mut lines: Vec<String> = Vec::new();
    lines.push("📂 Discovered resources for this session:".into());
    if resources.context_files.is_empty() {
        lines.push("  Context: (no context files loaded)".into());
    } else {
        lines.push(format!("  Context ({}):", resources.context_files.len()));
        lines.extend(
            resources
                .context_files
                .iter()
                .map(|path| format!("    • {path}")),
        );
    }

    if skills.is_empty() {
        lines.push(
            "  Skills: (none discovered — create .rpi/skills/ or ~/.rpi/agent/skills/)".into(),
        );
    } else {
        lines.push(format!("  Skills ({}):", skills.len()));
        for s in skills {
            let marker = if s.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            let desc: String = s.description.chars().take(72).collect();
            lines.push(format!("    • {}{marker} — {desc}", s.name));
        }
    }

    if templates.is_empty() {
        lines.push(
            "  Prompt templates: (none — create .rpi/prompts/ or ~/.rpi/agent/prompts/)".into(),
        );
    } else {
        lines.push(format!("  Prompt templates ({}):", templates.len()));
        for t in templates {
            let desc = t
                .description
                .as_deref()
                .unwrap_or("(no description)")
                .chars()
                .take(72)
                .collect::<String>();
            lines.push(format!("    • /{} — {desc}", t.name));
        }
    }
    lines.push(
        "  Use --no-skills/-ns, --no-prompt-templates/-np, --no-context-files/-nc to suppress."
            .into(),
    );
    lines
}

/// Append a multi-line neutral note (header line + body) to the chat container.
pub(super) fn container_note_block(container: &Arc<Container>, body: &str) {
    for line in body.lines() {
        container.add_child(Arc::new(Text::new(line.to_string(), 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}
