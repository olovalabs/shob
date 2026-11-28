/// Build a pane's input editor plus its change subscription.
///
/// The subscription defers its work: `InputEvent::Change` is emitted while the
/// input entity is leased, so ghost text and the completion menu are refreshed
/// after that effect cycle instead of re-entering the entity.
fn build_pane_input(
    pane_id: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> (Entity<InputState>, gpui::Subscription) {
    let input = cx.new(|cx| {
        let mut state = InputState::new(window, cx)
            .placeholder("Run commands")
            .auto_grow(1, 8);
        // Shell syntax highlighting, matching the block command language.
        state.set_highlighter("bash", cx);
        state
    });
    let subscription = cx.subscribe_in(
        &input,
        window,
        move |_this: &mut AppState, _input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                let weak = cx.entity().downgrade();
                window.defer(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.on_pane_input_changed(pane_id, window, cx);
                    });
                });
            }
        },
    );
    (input, subscription)
}

/// Replace the input value and place the cursor at the end.
fn set_input_text(
    input: &Entity<InputState>,
    text: String,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    let cursor = completion::utf16_len(&text);
    set_input_text_with_cursor(input, text, cursor, window, cx);
}

fn set_input_text_with_cursor(
    input: &Entity<InputState>,
    text: String,
    cursor_utf16: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    input.update(cx, |state, cx| {
        state.set_value(text, window, cx);
        let cursor = cursor_utf16.min(completion::utf16_len(state.value().as_str()));
        let position = state.text().offset_to_position(cursor);
        state.set_cursor_position(position, window, cx);
    });
}

fn move_input_cursor(
    input: &Entity<InputState>,
    cursor_utf16: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    input.update(cx, |state, cx| {
        let cursor = cursor_utf16.min(completion::utf16_len(state.value().as_str()));
        let position = state.text().offset_to_position(cursor);
        state.set_cursor_position(position, window, cx);
    });
}

/// Byte offset where the word before `cursor` starts (readline `Ctrl+W`).
fn previous_word_start(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    let mut start = cursor;
    while let Some(ch) = value[..start].chars().next_back() {
        if !ch.is_whitespace() {
            break;
        }
        start -= ch.len_utf8();
    }
    while let Some(ch) = value[..start].chars().next_back() {
        if ch.is_whitespace() {
            break;
        }
        start -= ch.len_utf8();
    }
    start
}

/// Byte offset just past the word at `cursor` (readline `Alt+F`).
fn next_word_end(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    let mut end = cursor;
    while let Some(ch) = value[end..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    while let Some(ch) = value[end..].chars().next() {
        if ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

fn detect_shell() -> (String, String) {
    if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            "cmd".to_string(),
        )
    } else {
        if let Ok(shell) = std::env::var("SHELL") {
            let name = std::path::Path::new(&shell)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shell".to_string());
            return (shell, name);
        }
        ("/bin/bash".to_string(), "bash".to_string())
    }
}
#[cfg(target_os = "linux")]
fn ensure_linux_desktop_entry() {
    if std::env::var_os("SHOB_SKIP_DESKTOP_ENTRY").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let apps_dir = data_dir.join("applications");
    let icons_512_dir = data_dir.join("icons/hicolor/512x512/apps");
    let _ = std::fs::create_dir_all(&apps_dir);
    let _ = std::fs::create_dir_all(&icons_512_dir);

    let icon_path = icons_512_dir.join("shob-brand-v3.png");
    let needs_icon_update = match std::fs::read(&icon_path) {
        Ok(existing) => existing != APP_ICON_PNG,
        Err(_) => true,
    };
    if needs_icon_update {
        let _ = std::fs::write(&icon_path, APP_ICON_PNG);
        if let Ok(img) = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)
        {
            for size in [256, 128, 64, 48, 32, 16] {
                let size_dir = data_dir.join(format!("icons/hicolor/{size}x{size}/apps"));
                let _ = std::fs::create_dir_all(&size_dir);
                let target = size_dir.join("shob-brand-v3.png");
                let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                let _ = resized.save_with_format(&target, image::ImageFormat::Png);
            }
        }
    }

    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("shob"));
    let exe_str = exe_path.to_string_lossy();

    let desktop_content = format!(
        "[Desktop Entry]\n\
        Type=Application\n\
        Name=Shob\n\
        Comment=GPU-accelerated terminal\n\
        Exec={exe_str} gui\n\
        Icon=shob-brand-v3\n\
        Terminal=false\n\
        Categories=System;TerminalEmulator;\n\
        StartupWMClass=shob\n"
    );

    let desktop_path = apps_dir.join("shob.desktop");
    if std::fs::read(&desktop_path).ok().as_deref() != Some(desktop_content.as_bytes()) {
        let _ = std::fs::write(desktop_path, desktop_content);
    }
}

fn startup_mark(label: &str) {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    if std::env::var_os("SHOB_STARTUP_TIMINGS").is_some() {
        eprintln!(
            "[startup] {label}: {:.2}ms",
            START
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_secs_f64()
                * 1000.0
        );
    }
}

fn main() -> Result<ExitCode> {
    startup_mark("process entry");
    let mut args = std::env::args().skip(1).peekable();
    match args.peek().map(String::as_str) {
        Some("report") => cli::run_report(&args.skip(1).collect::<Vec<_>>()),
        Some("skill") => cli::run_skill(&args.skip(1).collect::<Vec<_>>()),
        Some("integration") => integrations::run(&args.skip(1).collect::<Vec<_>>()),
        Some("status-hook") => integrations::run_hook(&args.skip(1).collect::<Vec<_>>()),
        Some("status-event") => cli::run_status_event(),
        Some("status") => cli::run_status(&args.skip(1).collect::<Vec<_>>()),
        Some("update") => updater::run_cli(args.skip(1)),
        Some("token-bench") => {
            let code = token_efficiency::bench_main::run(args.skip(1));
            return match code {
                std::process::ExitCode::SUCCESS => Ok(ExitCode::SUCCESS),
                _ => anyhow::bail!("token-efficiency benchmark failed"),
            };
        }
        Some("pane") => {
            #[cfg(unix)]
            {
                shob_compat::run_cli(&args.skip(1).collect::<Vec<_>>())
            }
            #[cfg(not(unix))]
            anyhow::bail!("native reporting compatibility requires a Unix socket");
        }
        Some("--help" | "-h") => {
            agent_cli::args::print_help();
            println!(
                "\nShob commands:\n  gui                         Open the desktop workspace\n  tui [options]               Run the agent CLI explicitly\n  update [--check]            Check for or install a Shob update\n  token-bench [--fixtures DIR] [--out FILE] Run token-efficiency benchmarks\n  report|skill|integration|status|status-event|status-hook\n  pane <report-agent|report-agent-session|release-agent> ..."
            );
            Ok(())
        }
        Some("--version" | "-V" | "-v") => {
            println!("shob {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("gui") => {
            anyhow::ensure!(args.nth(1).is_none(), "usage: shob gui");
            run_gui()
        }
        None if launched_by_cargo() || launched_without_terminal() => run_gui(),
        _ => {
            if args.peek().is_some_and(|arg| arg == "tui") {
                args.next();
            }
            if args.peek().is_some_and(|arg| arg == "update") {
                updater::run_cli(args.skip(1))?;
                return Ok(ExitCode::SUCCESS);
            }
            if updater::enabled() {
                agent_cli::updates::set_app_update_check(|| {
                    Box::pin(smol::unblock(updater::tui_notice))
                });
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let code = runtime.block_on(agent_cli::app::run_with_args(args.collect()));
            return Ok(ExitCode::from(code as u8));
        }
    }
    .map(|()| ExitCode::SUCCESS)
}

fn launched_by_cargo() -> bool {
    std::env::var_os("CARGO_MANIFEST_DIR").is_some_and(|dir| dir == env!("CARGO_MANIFEST_DIR"))
}

fn launched_without_terminal() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;

        // Explorer starts console binaries in a new console of their own.
        let mut processes = [0; 1];
        let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 1) };
        if count != 0 {
            return count == 1;
        }
    }
    !std::io::stdin().is_terminal()
        && !std::io::stdout().is_terminal()
        && !std::io::stderr().is_terminal()
}

fn run_gui() -> Result<()> {
    embedded::set_tool_arrival_hook(crate::stream_timing::tool_arrived);
    embedded::set_tool_end_hook(crate::stream_timing::tool_finished);
    embedded::set_tool_output_hook(crate::stream_timing::tool_output_arrived);
    let startup_error = Rc::new(std::cell::Cell::new(None));
    let report_error = Rc::clone(&startup_error);
    gpui_platform::application()
        .with_assets(CombinedAssets)
        .run(move |cx: &mut App| {
            #[cfg(target_os = "linux")]
            std::thread::spawn(ensure_linux_desktop_entry);
            if let Err(error) = paths::ensure_dirs() {
                eprintln!("failed to create app data directories: {error}");
            }
            #[cfg(target_os = "linux")]
            if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
                let local_bin = home.join(".local/bin");
                if local_bin.exists()
                    && let Ok(current_path) = std::env::var("PATH")
                {
                    let local_bin_str = local_bin.to_string_lossy();
                    if !current_path.split(':').any(|p| p == local_bin_str) {
                        unsafe {
                            std::env::set_var("PATH", format!("{local_bin_str}:{current_path}"));
                        }
                    }
                }
            }
            let app_settings = settings::load();
            startup_mark("settings loaded");
            let restore_disabled = std::env::var_os("SHOB_DISABLE_RESTORE").is_some();
            let restore_enabled = app_settings.restore.sessions && !restore_disabled;
            let load_history = app_settings.restore.agent_history && restore_enabled;
            let load_scrollback = app_settings.restore.scrollback;
            let window_icon = image::load_from_memory(APP_ICON_PNG).ok().map(|image| Arc::new(image.into_rgba8()));
            gpui_component::init(cx);
            // Shob uses two gpui-component generations: its own 0.5.1 fork
            // and the 0.9 stack behind gpui-kit. Their action names collide, so
            // initialize only gpui-kit's theme layer (globals + TextView
            // defaults) and leave its action-registering inits alone.
            gpui_kit::component::theme::init(cx);
            gpui_kit::component::GlobalState::init(cx);
            init_shob_input_bindings(cx);
            let initial_mode = app_settings.theme.resolve_mode(cx.window_appearance());
            theme::apply_component_theme(initial_mode, None, cx);
            load_embedded_fonts(cx);
            sync_component_fonts(cx);
            theme::sync_imported_components(
                app_settings.palette_scheme,
                app_settings.theme_colors.tokens(initial_mode, app_settings.palette_scheme),
                cx,
            );

            std::thread::spawn(|| {
                completion::prewarm();
                let _ = shell_history::cached();
            });

            startup_mark("components and fonts initialized");
            let palette = app_settings.theme_colors.palette(app_settings.palette_scheme, initial_mode.is_dark());
            let persistence_path = Some(paths::workspace_path());

            let displays: Vec<Bounds<Pixels>> = cx
                .displays()
                .iter()
                .map(|display| display.visible_bounds())
                .collect();
            let bounds = settings::window_bounds(app_settings.window, &displays);
            if let Err(error) = cx.open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    window_min_size: Some(settings::minimum_window_size()),
                    window_decorations: Some(WindowDecorations::Client),
                    app_id: Some("shob".to_string()),
                    icon: window_icon,
                    ..window_chrome::window_options()
                },
                |window, cx| {
                    startup_mark("window created");
                    let app_state = cx.new(|cx| AppState {
                        projects: Vec::new(),
                        scratch_dir: paths::scratch_dir(),
                        active_project_idx: 0,
                        next_id: 1,
                        session_index: HashMap::new(),
                        navigation_history: Vec::new(),
                        navigation_position: 0,
                        pane_index: HashMap::new(),
                        mounted_chat_panes: std::cell::RefCell::new(Vec::new()),
                        project_change_counts: HashMap::new(),
                        git_snapshots: HashMap::new(),
                        conversation_header_branch: None,
                        conversation_header_menu_focus: cx.focus_handle(),
                        git_requests: HashMap::new(),
                        git_request_serial: 0,
                        git_worktree_creating: HashSet::new(),
                        palette: palette.clone(),
                        focused_pane_id: None,
                        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
                        visible_browser: None,
                        sidebar_visible: app_settings.sidebar.visible,
                        compact_sidebar_open: false,
                        sidebar_focus: cx.focus_handle(),
                        sidebar_navigation: Default::default(),
                        sidebar_selection: Default::default(),
                        sidebar_inbox_scroll: gpui::ScrollHandle::new(),
                        sidebar_presentation: SidebarPresentation::default(),
                        sidebar_folder_show_all: HashSet::new(),
                        sidebar_folder_menu: None,
                        sidebar_menu_focus: cx.focus_handle(),
                        sidebar_search: None,
                        sidebar_project_picker: None,
                        sidebar_thread_menu: None,
                        sidebar_custom_snooze: None,
                        sidebar_thread_rename: None,
                        system_snapshot: system_detection::Snapshot::default(),
                        system_monitor_open: false,
                        sidebar_threads: HashMap::new(),
                        bots: Default::default(),
                        bot_form: None,
                        settled_visible_count: SIDEBAR_SETTLED_INITIAL_COUNT,
                        active_view: ActiveView::Workspace,
                        theme_preference: app_settings.theme,
                        palette_scheme: app_settings.palette_scheme,
                        agent_catalog: AgentCatalog::default(),
                        integration_panel_visible: false,
                        integration_feedback: SharedString::from("Setup is opt-in. Status becomes Reporting only after a live event arrives."),
                        sidebar_width: px(app_settings.sidebar.width).max(SIDEBAR_MIN_WIDTH),
                        sidebar_resize_offset: None,
                        sidebar_resizing: false,
                        sidebar_resize_collapse: false,
                        persistence_path: persistence_path.clone(),
                        save_generation: Arc::new(AtomicU64::new(0)),
                        workspace_save_task: None,
                        agent_preferences_generation: Arc::new(AtomicU64::new(0)),
                        agent_preferences_save_task: None,
                        scrollback_generation: Arc::new(AtomicU64::new(0)),
                        autosave_task: None,
                        pending_session_deletions: HashSet::new(),
                        workspace_cleared: false,
                        clear_data_running: false,
                        startup_loaded: false,
                        quit_subscription: None,
                        window_active: true,
                        status_primed: false,
                        pending_alerts: Vec::new(),
                        attention_seq: 0,
                        project_menu: None,
                        editor_picker_open: None,
                        project_script_form: None,
                        available_editors: Vec::new(),
                        renaming_project: None,
                        terminal_context_menu: None,
                        history_search: None,
                        image_viewer: None,
                        settings: app_settings.clone(),
                        dirty: false,
                        last_window_bounds: app_settings.window,
                        persistence_enabled: std::env::var_os("SHOB_DISABLE_RESTORE").is_none(),
                        settings_section: SettingsSection::default(),
                        settings_focus: cx.focus_handle(),
                        browser_install: BrowserInstallState::Checking,
                        update_state: updater::State::default(),
                        clear_data_confirm: false,
                    });
                    app_state.update(cx, |state, cx| {
                        state.ensure_sidebar_search(window, cx);
                        state.observe_appearance(window, cx);
                        state.refresh_project_editors(cx);
                        state.check_browser_install(cx);
                    });

                    // Close button / window-manager close: flush everything
                    // synchronously before the window goes away.
                    let close_state = app_state.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        if let Some(state) = close_state.upgrade() {
                            state.update(cx, |state, cx| {
                                state.save_all_sync(Some(window), cx);
                            });
                        }
                        true
                    });

                    let weak_state = app_state.downgrade();
                    let load_path = persistence_path.clone();
                    let restore_window = window.window_handle();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        let restored = cx
                            .background_executor()
                            .spawn(async move {
                                let Some(path) = load_path else {
                                    return RestoreData {
                                        workspace: workspace_state::SavedWorkspace::default(),
                                        scrollbacks: HashMap::new(),
                                        transcripts: HashMap::new(),
                                        usage: HashMap::new(),
                                    };
                                };
                                if let Err(error) = workspace_state::retry_pending_deletions(&path) {
                                    eprintln!("failed to retry session deletion: {error}");
                                }
                                let mut workspace = if restore_disabled {
                                    workspace_state::SavedWorkspace::default()
                                } else {
                                    let mut workspace = workspace_state::load(&path);
                                    if !restore_enabled {
                                        workspace.projects.clear();
                                        workspace.threads.clear();
                                        workspace.active_project = None;
                                    }
                                    workspace
                                };
                                if let Err(error) = workspace_state::hide_pending_deletions(&mut workspace) {
                                    eprintln!("failed to read pending session deletions: {error}");
                                }
                                AppState::prepare_bot_workspaces(&workspace);
                                let mut scrollbacks = HashMap::new();
                                let mut transcripts = HashMap::new();
                                let mut usage = HashMap::new();
                                let mut live_uids = HashSet::new();
                                for project in &workspace.projects {
                                    for session in &project.sessions {
                                        if project.projectless
                                            && let Err(error) = std::fs::create_dir_all(project.path.join(&session.uid))
                                        {
                                            eprintln!("failed to restore scratch directory: {error}");
                                        }
                                        for pane in &session.panes {
                                            live_uids.insert(pane.uid.clone());
                                            if load_scrollback && let Some(text) =
                                                workspace_state::read_scrollback(&pane.uid)
                                            {
                                                scrollbacks.insert(pane.uid.clone(), text);
                                            }
                                            if load_history {
                                                let keys = std::iter::once(
                                                    pane.chat.valid_conversation_key()
                                                        .map(str::to_owned)
                                                        .unwrap_or_else(|| conversations::conversation_key(None)),
                                                ).chain(pane.chat.models.iter().map(|legacy| {
                                                    conversations::legacy_conversation_key(legacy.model.as_ref())
                                                }));
                                                for key in keys {
                                                    let conversation_path = paths::conversations_dir()
                                                        .join(&pane.uid)
                                                        .join(format!("{key}.jsonl"));
                                                    let restored = conversations::restore(&conversation_path);
                                                    if let Some(total) = conversations::load_usage(&conversation_path) {
                                                        usage.insert(transcript_key(&pane.uid, &key), total);
                                                    }
                                                    if !restored.messages.is_empty() || restored.checklist.is_some() {
                                                        transcripts.insert(
                                                            transcript_key(&pane.uid, &key),
                                                            restored,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Never prune from an empty/malformed workspace:
                                // that would delete scrollback for every pane.
                                if restore_enabled && !workspace.projects.is_empty() {
                                    workspace_state::prune_stale_scrollbacks(&live_uids);
                                }
                                RestoreData {
                                    workspace,
                                    scrollbacks,
                                    transcripts,
                                    usage,
                                }
                            })
                            .await;
                        startup_mark("restore data loaded");
                        let _ = weak_state.update(cx, |state, cx| {
                            state.apply_saved_workspace(restored, cx);
                            if let Some(id) = state.focused_pane_id {
                                let state = cx.entity().downgrade();
                                cx.defer(move |cx| {
                                    let _ = restore_window.update(cx, |_, window, cx| {
                                        let _ = state.update(cx, |state, cx| {
                                            if !state.pane_uses_agent_layout(id)
                                                || !state.active_session().and_then(|session| state.one_mode_pane(session)).is_some_and(|pane| pane.chat.active)
                                            {
                                                state.focus_workspace_pane(id, window, cx);
                                                state.schedule_workspace_save(cx);
                                            }
                                        });
                                    });
                                });
                            }
                            if updater::enabled() {
                                state.check_for_updates(cx);
                            }
                        });
                        startup_mark("restore applied");
                    })
                    .detach();

                    app_state.update(cx, |state, cx| {
                        state.persistence_path = persistence_path.clone();
                        state.startup_loaded = false;
                        state.quit_subscription = Some(cx.on_app_quit(move |state, cx| {
                            state.save_all_sync(None, cx);
                            async {}
                        }));
                    });

                    // Periodic snapshot: bounds the amount of work lost if the
                    // process is killed instead of closed.
                    let autosave_state = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(30))
                                .await;
                            match autosave_state.update(cx, |state, cx| state.autosave(cx)) {
                                Ok(()) => {}
                                Err(_) => break,
                            }
                        }
                    })
                    .detach();

                    // Wake-driven agent pump. Bridges signal a capacity-1 channel
                    // as soon as they queue an event, so an update reaches the UI
                    // on the next frame instead of waiting for a fixed 16 ms tick.
                    // A low-frequency ticker remains as a safety net for events
                    // produced by backends that do not signal.
                    let (wake_tx, wake_rx) = smol::channel::bounded::<()>(1);
                    crate::agent_bridge::set_wake({
                        let wake_tx = wake_tx.clone();
                        std::sync::Arc::new(move || {
                            let _ = wake_tx.try_send(());
                        })
                    });
                    // Populate model capabilities off the UI thread, including
                    // for a model restored before the picker is opened.
                    app_state.update(cx, |state, _| state.agent_catalog.refresh());
                    let agent_streaming =
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    {
                        let tick_tx = wake_tx.clone();
                        let agent_streaming = agent_streaming.clone();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            loop {
                                let interval = if agent_streaming
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    50
                                } else {
                                    250
                                };
                                cx.background_executor()
                                    .timer(std::time::Duration::from_millis(interval))
                                    .await;
                                if matches!(
                                    tick_tx.try_send(()),
                                    Err(smol::channel::TrySendError::Closed(()))
                                ) {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }
                    let agent_state_weak = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            if wake_rx.recv().await.is_err() {
                                break;
                            }
                            // The capacity-one channel coalesces pending wakes.
                            // Leave any new wake queued while applying this batch.
                            match agent_state_weak
                                .update(cx, |state, cx| state.poll_agent_events(cx))
                            {
                                Ok(active) => agent_streaming
                                    .store(active, std::sync::atomic::Ordering::Relaxed),
                                Err(_) => break,
                            }
                            crate::stream_timing::maybe_report();
                            // Return to the platform event loop between bounded
                            // drains, without adding a token-delivery timer.
                            smol::future::yield_now().await;
                        }
                    }).detach();

                    let app_state_weak = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            cx.background_executor().timer(POLL_INTERVAL).await;

                            // Snapshot the shell pids on the UI thread (a borrow
                            // over the pane list), do the blocking `/proc`
                            // inspection on the background executor, then apply
                            // the result back on the UI thread. Nothing in this
                            // loop reads files or waits on I/O while a frame
                            // could be building.
                            let Ok(pids) = app_state_weak.update(cx, |state, _| state.pane_pids())
                            else {
                                break;
                            };
                            let foregrounds = cx
                                .background_executor()
                                .spawn(async move {
                                    pids.into_iter()
                                        .map(|pid| {
                                            (pid, status::foreground_process_names(Some(pid)))
                                        })
                                        .collect::<HashMap<u32, Arc<[String]>>>()
                                })
                                .await;

                            let res = app_state_weak.update(cx, |state, cx| {
                                state.poll_sessions_status_with(&foregrounds, cx);
                            });
                            if res.is_err() {
                                break;
                            }
                        }
                    })
                    .detach();

                    let path_state = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            let Ok(requests) = path_state.update(cx, |state, _| state.active_path_requests()) else {
                                break;
                            };
                            if !requests.is_empty() {
                                let samples = cx.background_executor().spawn(async move {
                                    requests.into_iter().map(|(id, probe)| (id, probe.sample())).collect::<Vec<_>>()
                                }).await;
                                if path_state.update(cx, |state, cx| state.apply_path_samples(samples, cx)).is_err() {
                                    break;
                                }
                            }
                            cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                        }
                    }).detach();

                    let system_state = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        let mut sampler = cx.background_executor().spawn(async { system_detection::Sampler::new() }).await;
                        loop {
                            cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
                            let (next, snapshot) = cx.background_executor().spawn(async move {
                                let snapshot = sampler.sample();
                                (sampler, snapshot)
                            }).await;
                            sampler = next;
                            if system_state.update(cx, |state, cx| {
                                if state.system_snapshot != snapshot {
                                    state.system_snapshot = snapshot;
                                    if state.system_monitor_open && (state.sidebar_visible || state.compact_sidebar_open) { cx.notify(); }
                                }
                            }).is_err() { break; }
                        }
                    }).detach();

                    // Refresh conversation HEAD and agent workspace details without overlapping background requests.
                    let changes_state = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            let Ok(directories) = changes_state.update(cx, |state, cx| {
                                if state.active_view != ActiveView::Workspace { return None; }
                                let header_dir = state.conversation_header_directory().map(Path::to_path_buf);
                                let agent_dir = state.active_session().and_then(|session| {
                                    let mut panes = session.panes.iter().filter(|p| p.chat.active && p.preview_owner.is_none());
                                    panes.clone().find(|p| Some(p.id) == state.focused_pane_id)
                                        .or_else(|| panes.next())
                                        .map(|pane| state.agent_working_directory(pane))
                                });
                                if let Some(dir) = &agent_dir {
                                    state.schedule_git_refresh(dir.clone(), cx);
                                }
                                Some((header_dir, agent_dir))
                            }) else { break; };
                            if let Some((header_dir, agent_dir)) = directories
                                && (header_dir.is_some() || agent_dir.is_some())
                            {
                                let (branch, agent) = cx.background_executor().spawn(async move {
                                    let branch = header_dir.and_then(|dir| {
                                        git_workspace::detect_git_branch(&dir)
                                            .map(|branch| (dir, SharedString::from(branch)))
                                    });
                                    let agent = agent_dir.map(|dir| {
                                        let summary = project_diff::summary(dir.clone()).ok();
                                        (dir, summary)
                                    });
                                    (branch, agent)
                                }).await;
                                if changes_state.update(cx, |state, cx| {
                                    if state.conversation_header_branch != branch {
                                        state.conversation_header_branch = branch;
                                        cx.notify();
                                    }
                                    if let Some((directory, summary)) = agent {
                                        if state.project_change_counts.get(&directory) != Some(&summary) {
                                            state.project_change_counts.insert(directory, summary);
                                            cx.notify();
                                        }
                                    }
                                }).is_err() { break; }
                            }
                            cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                        }
                    }).detach();

                    // Opt-in diagnostics: one line per second with frame
                    // timing, UI-thread work, CPU, RSS and wakeups. Not spawned
                    // at all unless `SHOB_DIAGNOSTICS` is set.
                    if diagnostics::enabled() {
                        let memory_state = app_state.downgrade();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let mut sampler = diagnostics::Sampler::new();
                            loop {
                                cx.background_executor()
                                    .timer(diagnostics::REPORT_INTERVAL)
                                    .await;
                                let memory = memory_state
                                    .update(cx, |state, cx| state.memory_report(cx).line())
                                    .ok()
                                    .unwrap_or_default();
                                eprintln!("{}\n{}", sampler.report(), memory);
                            }
                        })
                        .detach();
                    }

                    // TEMPORARY DIAGNOSTIC: SHOB_RESIZE_PROBE=<rounds> drags
                    // the window edge in small steps, the way a real resize
                    // does, while the status poller keeps ticking.
                    if let Some(probe) = std::env::var_os("SHOB_RESIZE_PROBE").or_else(|| std::env::var_os("MARKUP_RESIZE_PROBE")) {
                        let rounds: usize = probe.to_string_lossy().parse().unwrap_or(10);
                        let handle = window.window_handle();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let (mut w, mut h) = (1200.0f32, 780.0f32);
                            for _ in 0..rounds {
                                for _ in 0..60 {
                                    w = (w - 12.0).max(360.0);
                                    h = (h - 8.0).max(240.0);
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(16))
                                        .await;
                                    if handle
                                        .update(cx, |_, window, _| {
                                            window.resize(size(px(w), px(h)));
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                for _ in 0..60 {
                                    w = (w + 12.0).min(1200.0);
                                    h = (h + 8.0).min(780.0);
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(16))
                                        .await;
                                    if handle
                                        .update(cx, |_, window, _| {
                                            window.resize(size(px(w), px(h)));
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            eprintln!("[probe] {rounds} drag rounds finished without a panic");
                            cx.update(|cx| cx.quit());
                        })
                        .detach();
                    }

                    let link_host = app_state.downgrade();
                    cx.set_global(gpui_component::text::TextLinkHandler(std::rc::Rc::new(move |link, window, cx| {
                        let mut handled = false;
                        let _ = link_host.update(cx, |state, cx| {
                            if let Some(pane_id) = state.focused_pane_id {
                                handled = state.open_local_link(pane_id, link, window, cx);
                            }
                        });
                        handled
                    })));
                    confirmation_prompt::install(app_state.downgrade(), cx);
                    cx.new(|cx| Root::new(app_state, window, cx))
                },
            ) {
                report_error.set(Some(error.context("Could not open the Shob window")));
                cx.quit();
                return;
            }

            cx.activate(true);
        });

    startup_error.take().map_or(Ok(()), Err)
}
