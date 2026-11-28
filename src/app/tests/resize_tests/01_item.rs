use super::*;
use gpui::{HeadlessAppContext, Modifiers, VisualContext, VisualTestContext, point, size};
use std::ops::{Deref, DerefMut};

// VisualTestContext supplies input simulation, while HeadlessAppContext is
// the public constructor that installs a real renderer. Point the visual
// context at the headless app and its executors; keep the owner alive for
// the duration of each test so it can shut the app down afterward.
pub(crate) struct TestAppContext {
    visual: gpui::TestAppContext,
    _headless: HeadlessAppContext,
}

impl TestAppContext {
    pub(crate) fn new() -> Self {
        let text = gpui_platform::current_platform(true).text_system();
        assert!(
            !text.all_font_names().is_empty(),
            "headless visual tests require a real text system and installed fonts; enable gpui-platform's x11 or wayland feature on Linux"
        );
        let headless =
            HeadlessAppContext::with_platform(text.clone(), Arc::new(CombinedAssets), || {
                let renderer = gpui_platform::current_headless_renderer()?
                    .ok_or_else(|| anyhow::anyhow!("headless GPU renderer unavailable"))?;
                Ok(Some(renderer))
            });
        let mut visual =
            gpui::TestAppContext::build_with_text_system(gpui::TestDispatcher::new(0), None, text);
        visual.app = headless.app.clone();
        visual.background_executor = headless.background_executor.clone();
        visual.foreground_executor = headless.foreground_executor.clone();
        Self {
            visual,
            _headless: headless,
        }
    }
}

impl Deref for TestAppContext {
    type Target = gpui::TestAppContext;

    fn deref(&self) -> &Self::Target {
        &self.visual
    }
}

impl DerefMut for TestAppContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.visual
    }
}

pub(crate) trait DrawFrame {
    fn draw_frame(&mut self) -> anyhow::Result<()>;
    fn draw_commands(&mut self);
}

impl DrawFrame for VisualTestContext {
    fn draw_frame(&mut self) -> anyhow::Result<()> {
        self.update(|window, cx| {
            window.draw(cx).clear(cx);
            let rendered = window.render_to_image().map(drop);
            // Production runs next-frame callbacks as soon as the frame is
            // presented; make headless frames do the same so paint-timing
            // assertions observe the real ordering.
            window.simulate_next_frame(cx);
            rendered
        })
    }

    fn draw_commands(&mut self) {
        self.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_next_frame(cx);
        });
    }
}

/// One project with one live session, built exactly like `main` does.
fn build_app_state(cx: &mut Context<AppState>) -> AppState {
    let palette = ColorPalette::default();
    let dir = std::env::current_dir().expect("current dir");
    let weak = cx.entity().downgrade();
    let (terminal, pid, generation, terminal_subscription) = AppState::spawn_terminal_view(
        Some(&dir),
        palette.clone(),
        weak,
        1,
        None,
        settings::MemorySettings::default().scrollback(),
        cx,
    )
    .expect("terminal view");
    let title = SharedString::from("terminal 1");

    let mut pane = TerminalPane::new(1, generation, terminal, title.clone(), title.clone(), pid);
    pane.terminal_subscription = Some(terminal_subscription);
    // Tests assert exact history indices; the developer machine's real
    // shell history must not leak into that.
    pane.shell_history = Arc::new(Vec::new());

    AppState {
        projects: vec![Project {
            id: 1,
            name: SharedString::from("shob"),
            path: dir,
            projectless: false,
            sessions: vec![Session {
                id: 1,
                uid: paths::uid("s"),
                scratch_dir: None,
                title: title.clone(),
                display_title: title,
                logo: logos::default_logo(),
                pane_tree: Some(PaneNode::Leaf(1)),
                maximized_pane_id: None,
                panes: vec![pane],
                status: SessionStatus::Idle,
            }],
            active_session_idx: 0,
            expanded: true,
            visible_session_count: 5,
        }],
        scratch_dir: paths::scratch_dir(),
        active_project_idx: 0,
        next_id: 2,
        session_index: HashMap::from([(1, (0, 0))]),
        navigation_history: Vec::new(),
        navigation_position: 0,
        pane_index: HashMap::from([(1, (0, 0, 0))]),
        mounted_chat_panes: std::cell::RefCell::new(Vec::new()),
        project_change_counts: HashMap::new(),
        git_snapshots: HashMap::new(),
        conversation_header_branch: None,
        conversation_header_menu_focus: cx.focus_handle(),
        git_requests: HashMap::new(),
        git_request_serial: 0,
        git_worktree_creating: HashSet::new(),
        palette,
        focused_pane_id: Some(1),
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        visible_browser: None,
        sidebar_visible: true,
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
        settled_visible_count: SIDEBAR_SETTLED_INITIAL_COUNT,
        active_view: ActiveView::Workspace,
        theme_preference: ThemePreference::System,
        palette_scheme: PaletteScheme::default(),
        agent_catalog: AgentCatalog::default(),
        integration_panel_visible: false,
        integration_feedback: SharedString::from(
            "Setup is opt-in. Status becomes Reporting only after a live event arrives.",
        ),
        sidebar_width: SIDEBAR_WIDTH,
        sidebar_resize_offset: None,
        sidebar_resizing: false,
        sidebar_resize_collapse: false,
        persistence_path: None,
        save_generation: Arc::new(AtomicU64::new(0)),
        workspace_save_task: None,
        agent_preferences_generation: Arc::new(AtomicU64::new(0)),
        agent_preferences_save_task: None,
        scrollback_generation: Arc::new(AtomicU64::new(0)),
        autosave_task: None,
        pending_session_deletions: HashSet::new(),
        workspace_cleared: false,
        clear_data_running: false,
        startup_loaded: true,
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
        settings: settings::AppSettings {
            agent_layout: false,
            ..Default::default()
        },
        dirty: false,
        last_window_bounds: None,
        persistence_enabled: false,
        settings_section: SettingsSection::default(),
        settings_focus: cx.focus_handle(),
        browser_install: BrowserInstallState::NotInstalled,
        update_state: updater::State::default(),
        clear_data_confirm: false,
    }
}

/// Open the app the way `main` does and return a context that can draw it.
pub(crate) fn open_app(cx: &mut TestAppContext) -> (VisualTestContext, Entity<AppState>) {
    load_provider_icons();
    cx.update(|cx| {
        gpui_component::init(cx);
        gpui_kit::component::theme::init(cx);
        gpui_kit::component::GlobalState::init(cx);
        init_shob_input_bindings(cx);
        theme::apply_component_theme(gpui_component::ThemeMode::Dark, None, cx);
        load_embedded_fonts(cx);
        sync_component_fonts(cx);
    });

    let state = cx.new(build_app_state);
    let handle = cx
        .update(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                    window_decorations: Some(WindowDecorations::Client),
                    app_id: Some("shob".to_string()),
                    icon: image::load_from_memory(APP_ICON_PNG)
                        .ok()
                        .map(|image| Arc::new(image.into_rgba8())),
                    ..window_chrome::window_options()
                },
                |window, cx| {
                    state.update(cx, |state, cx| {
                        state.ensure_sidebar_search(window, cx);
                        state.observe_appearance(window, cx);
                    });
                    cx.new(|cx| Root::new(state.clone(), window, cx))
                },
            )
        })
        .expect("open window");

    let cx = VisualTestContext::from_window(*handle.deref(), cx);
    cx.run_until_parked();
    (cx, state)
}

#[test]
fn themed_welcome_artwork_fits_resized_windows() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.settings.agent_layout = true;
        s.palette_scheme = PaletteScheme::MidnightGarden;
        s.theme_preference = ThemePreference::Dark;
        s.settings.theme_colors =
            theme::ThemeColors::complete_defaults(PaletteScheme::MidnightGarden);
        s.projects[0].sessions[0].panes[0].chat.active = true;
        s.apply_theme_preference(Some(window), cx);
        cx.notify();
    });
    for (width, height) in [
        (1672.0, 940.0),
        (900.0, 650.0),
        (600.0, 400.0),
        (900.0, 1200.0),
        (2400.0, 650.0),
    ] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.draw_frame().unwrap();
        let artwork = cx
            .debug_bounds("theme-ascii-glow")
            .expect("ASCII background");
        let composer = cx.debug_bounds("shob-input-bar").expect("welcome composer");
        assert!(
            cx.debug_bounds("focused-agent-info").is_none(),
            "welcome screen hides project details"
        );
        assert!(artwork.size.height <= px(height + 4.0));
        assert!(artwork.size.width <= px(width));
        assert!(artwork.left() >= px(0.0));
        assert!(
            (artwork.right() - px(width)).abs() <= px(4.0),
            "artwork fills the workspace width: {artwork:?}, window width {width}"
        );
        assert!(composer.left() >= px(0.0));
        assert!(composer.right() <= px(width));
        assert!(composer.bottom() <= px(height));
        if let Some(dir) = std::env::var_os("SHOB_WELCOME_SCREENSHOT_DIR") {
            for _ in 0..4 {
                cx.run_until_parked();
                cx.draw_frame().unwrap();
            }
            let path =
                std::path::PathBuf::from(dir).join(format!("welcome-{width:.0}x{height:.0}.png"));
            cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
        }
    }
}

#[test]
fn gradient_ascii_colors_provider_and_model_dialog_inputs() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.palette_scheme = PaletteScheme::MidnightGarden;
        s.theme_preference = ThemePreference::Dark;
        s.settings.theme_colors =
            theme::ThemeColors::complete_defaults(PaletteScheme::MidnightGarden);
        s.apply_theme_preference(Some(window), cx);
        s.open_agent_auth(1, AgentDialogPage::Auth, window, cx);
        let auth = s.pane_mut(1).unwrap().chat.auth.as_mut().unwrap();
        auth.methods_discovery = None;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    for selector in ["agent-dialog-filter", "auth-provider-id-input"] {
        let bounds = cx.debug_bounds(selector).expect("dialog input");
        let pixel = cx.update(|window, _| {
            let image = window.render_to_image().unwrap();
            let scale = window.scale_factor();
            let x = (f32::from(bounds.left() + px(5.0)) * scale) as u32;
            let y = (f32::from(bounds.center().y) * scale) as u32;
            *image.get_pixel(x, y)
        });
        assert!(
            pixel[2] > pixel[0] + 20 && pixel[2] > pixel[1],
            "{selector} should have the blue theme background: {pixel:?}"
        );
    }
    let filter = cx.debug_bounds("agent-dialog-filter").unwrap();
    cx.simulate_click(filter.center(), Modifiers::default());
    cx.simulate_input("openai");
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, cx| {
        let auth = s.pane_ref(1).unwrap().chat.auth.as_ref().unwrap();
        assert_eq!(auth.filter.read(cx).value().as_str(), "openai");
    });
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_agent_dialog_page(1, AgentDialogPage::Models, window, cx);
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-filter").is_some());
}

#[test]
fn gradient_ascii_colors_split_pane_input_for_terminal_and_agent() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.settings.agent_layout = false;
        s.palette_scheme = PaletteScheme::MidnightGarden;
        s.theme_preference = ThemePreference::Dark;
        s.settings.theme_colors =
            theme::ThemeColors::complete_defaults(PaletteScheme::MidnightGarden);
        s.apply_theme_preference(Some(window), cx);
        cx.notify();
    });
    for agent in [false, true] {
        cx.update_window_entity(&state, |s, _, cx| {
            s.projects[0].sessions[0].panes[0].chat.active = agent;
            cx.notify();
        });
        cx.draw_frame().unwrap();
        let bar = cx.debug_bounds("shob-input-bar").expect("input bar");
        let pixel = cx.update(|window, _| {
            let image = window.render_to_image().unwrap();
            let scale = window.scale_factor();
            let x = (f32::from(bar.left() + px(4.0)) * scale) as u32;
            let y = (f32::from(bar.bottom() - px(4.0)) * scale) as u32;
            *image.get_pixel(x, y)
        });
        assert!(
            pixel[2] > pixel[0] + 20 && pixel[2] > pixel[1],
            "the {agent:?} input surface should use the blue theme: {pixel:?}"
        );
    }
}

#[test]
fn settings_navigation_isolates_pages_and_returns_to_workspace() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    let settings = cx.debug_bounds("sidebar-settings-entry").unwrap();
    cx.simulate_click(settings.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("sidebar").is_none());
    assert!(cx.debug_bounds("theme-option-system").is_some());
    assert!(cx.debug_bounds("Shob").is_none());

    for (section, visible, hidden) in [
        ("settings-nav-restore", "restore-sessions", "archived-threads"),
        ("settings-nav-archive", "archived-threads", "restore-sessions"),
        ("settings-nav-storage", "clear-saved-data", "archived-threads"),
    ] {
        let link = cx.debug_bounds(section).unwrap();
        cx.simulate_click(link.center(), Modifiers::default());
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds(visible).is_some(), "missing {visible}");
        assert!(cx.debug_bounds(hidden).is_none(), "unexpected {hidden}");
    }
    let clear = cx.debug_bounds("clear-saved-data").unwrap();
    cx.simulate_click(clear.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("confirm-clear-saved-data").is_some());
    cx.simulate_keystrokes("escape");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("confirm-clear-saved-data").is_none());
    assert!(cx.debug_bounds("clear-saved-data").is_some());

    let back = cx.debug_bounds("settings-close-btn").unwrap();
    cx.simulate_click(back.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("settings-navigation").is_none());
    assert!(cx.debug_bounds("sidebar").is_some());
    state.read_with(&cx, |s, _| {
        assert_eq!(s.active_view, ActiveView::Workspace);
        assert_eq!(s.focused_pane_id, Some(1));
    });
}

#[test]
fn theme_picker_keeps_all_palettes_reachable_at_narrow_width() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.open_settings(window, cx);
        s.settings_section = SettingsSection::Themes;
    });
    for (width, columns) in [(1200.0, 3), (500.0, 2)] {
        cx.simulate_resize(size(px(width), px(1000.0)));
        cx.draw_frame().unwrap();
        let bounds: Vec<_> = PaletteScheme::ALL
            .iter()
            .map(|scheme| cx.debug_bounds(scheme.label()).unwrap())
            .collect();
        for row in bounds.chunks(columns) {
            assert!(row.windows(2).all(|pair| pair[0].right() < pair[1].left()));
        }
        assert!(bounds[0].bottom() < bounds[columns].top());
        assert!(bounds.iter().all(|card| card.right() <= px(width)));
        if let Some(path) = std::env::var_os("SHOB_THEMES_SCREENSHOT") {
            let path = PathBuf::from(path);
            let path = if columns == 3 { path } else { path.with_file_name("settings-compact.png") };
            cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
        }
        if columns == 3 {
            let matrix = cx.debug_bounds("Matrix").unwrap();
            cx.simulate_click(matrix.center(), Modifiers::default());
            cx.draw_frame().unwrap();
            state.read_with(&cx, |s, cx| {
                let tokens = s.tokens(cx);
                assert_eq!(s.saved_settings().palette_scheme, PaletteScheme::Matrix);
                assert_eq!(
                    terminal::hsla_to_rgb(s.palette.background()),
                    terminal::hsla_to_rgb(tokens.pane_bg),
                );
                assert_eq!(gpui_component::Theme::global(cx).colors.foreground, tokens.text_primary);
            });
        }
    }
}

#[test]
fn model_picker_opens_above_trigger_and_sets_reasoning() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    for agent_layout in [true, false] {
        cx.update_window_entity(&state, |s, _, cx| {
            s.theme_preference = ThemePreference::Dark;
            s.settings.agent_layout = agent_layout;
            let chat = &mut s.projects[0].sessions[0].panes[0].chat;
            chat.active = true;
            chat.selected_model = Some(agent_auth::AgentModel {
                provider: "google-antigravity".into(),
                id: "gemini-3.8-flash".into(),
                backend: agent_auth::ModelBackend::Shob,
            });
            s.apply_theme_preference(None, cx);
            cx.notify();
        });
        cx.draw_frame().unwrap();
        let trigger = cx.debug_bounds("select-agent-model-pill").unwrap();
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.update_window_entity(&state, |s, _, cx| {
            s.agent_catalog.data.providers = vec![agent_auth::Provider {
                name: None,
                custom: false,
                id: "google-antigravity".into(),
                ready: true,
                source: None,
            }];
            s.agent_catalog.data.models = vec![s.projects[0].sessions[0].panes[0]
                .chat
                .selected_model
                .clone()
                .unwrap()];
            cx.notify();
        });
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("picker-permissions").is_some());
        let picker = cx.debug_bounds("agent-model-panel").unwrap();
        assert!((trigger.top() - picker.bottom() - px(4.0)).abs() < px(1.0));
        assert!((trigger.right() - picker.right()).abs() < px(1.0));
        if agent_layout && let Some(path) = std::env::var_os("SHOB_MODEL_PICKER_SCREENSHOT") {
            cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
        }
        let medium = cx.debug_bounds("model-reasoning-medium").unwrap();
        cx.simulate_click(medium.center(), Modifiers::default());
        cx.draw_frame().unwrap();
        state.read_with(&cx, |s, _| {
            assert_eq!(
                s.projects[0].sessions[0].panes[0]
                    .chat
                    .effective_thinking_level(),
                "medium"
            );
        });
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("agent-model-panel").is_none());
    }
    cx.simulate_resize(size(px(520.0), px(405.0)));
    cx.simulate_scale_factor_change(1.0);
    cx.update_window_entity(&state, |s, window, cx| {
        s.theme_preference = ThemePreference::Light;
        s.apply_theme_preference(Some(window), cx);
    });
    cx.draw_frame().unwrap();
    let trigger = cx.debug_bounds("select-agent-model-pill").unwrap();
    cx.simulate_click(trigger.center(), Modifiers::default());
    cx.update_window_entity(&state, |s, _, cx| {
        let selected = s.projects[0].sessions[0].panes[0]
            .chat
            .selected_model
            .clone()
            .unwrap();
        s.agent_catalog.data.providers = vec![agent_auth::Provider {
            name: None,
            custom: false,
            id: selected.provider.clone(),
            ready: true,
            source: None,
        }];
        s.agent_catalog.data.models = (1..=12)
            .map(|index| agent_auth::AgentModel {
                id: format!("Example model {index:02}"),
                ..selected.clone()
            })
            .collect();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let picker = cx.debug_bounds("agent-model-panel").unwrap();
    assert!(picker.left() >= px(8.0) && picker.right() <= px(512.0));
    assert!(picker.top() >= px(8.0) && picker.bottom() <= px(397.0));
    let row = cx.debug_bounds("agent-model-0").unwrap();
    let manage = cx.debug_bounds("agent-more-providers").unwrap();
    assert!(row.bottom() < manage.top());
    if let Some(path) = std::env::var_os("SHOB_MODEL_PICKER_SCREENSHOT") {
        let path = PathBuf::from(path).with_file_name("shob-picker-light-narrow.png");
        cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn titlebar_close_responds_at_the_edge_of_the_button() {
    let mut app = TestAppContext::new();
    let (mut cx, _) = open_app(&mut app);
    let window = cx.update(|window, _| window.window_handle());
    cx.simulate_window_decorations_change(
        window,
        gpui::Decorations::Client {
            tiling: Default::default(),
        },
    );
    cx.draw_frame().unwrap();
    let close = cx.debug_bounds("close").expect("client close button");
    cx.simulate_click(
        point(close.left() + px(3.0), close.center().y),
        Modifiers::none(),
    );
    cx.run_until_parked();
    app.update(|cx| assert!(cx.windows().is_empty()));
}

#[test]
fn conversation_header_switcher_activates_a_session() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("conversation-session-switcher").is_none());
    cx.update_window_entity(&state, |s, window, cx| {
        s.settings.agent_layout = true;
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.active = true;
        pane.legacy_chat = true;
        s.create_project_session(0, true, window, cx);
        assert_eq!(s.projects[0].active_session_idx, 1);
    });
    cx.draw_frame().unwrap();
    let header = cx.debug_bounds("agent-conversation-header").unwrap();
    let switcher = cx.debug_bounds("conversation-session-switcher").unwrap();
    let info = cx.debug_bounds("focused-agent-info-toggle").unwrap();
    assert!(header.contains(&switcher.center()) && header.contains(&info.center()));
    cx.simulate_click(switcher.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let first = cx.debug_bounds("conversation-session-1").unwrap();
    cx.simulate_click(first.center(), Modifiers::none());
    assert_eq!(
        state.read_with(&cx, |s, _| s.projects[0].active_session_idx),
        0
    );
    cx.draw_frame().unwrap();
    let switcher = cx.debug_bounds("conversation-session-switcher").unwrap();
    cx.simulate_click(switcher.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        state.read_with(&cx, |s, _| s.projects[0].active_session_idx),
        1
    );
}

#[test]
fn focused_environment_panel_opens_on_narrow_windows() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.agent_layout = true;
        s.available_editors = vec![("Zed".into(), PathBuf::from("zed"))];
        s.projects[0].sessions[0].panes[0].chat.active = true;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("focused-agent-info").is_none(),
        "the welcome screen hides project details"
    );
    cx.update_window_entity(&state, |s, _, cx| {
        let messages = s.projects[0].sessions[0].panes[0].chat.visible_messages_mut();
        messages.push(Arc::new(ChatMessage::new(ChatRole::User, "Hello".into())));
        push_chat_event(messages, agent_bridge::AgentEvent::Subagents {
            tool_id: "workers".into(),
            workers: [ ("Scanning", "running"), ("Reviewed", "completed") ]
                .into_iter()
                .map(|(id, status)| subagent_chat::Worker {
                    id: id.into(),
                    title: id.into(),
                    status: status.into(),
                    transcript: None,
                    result: None,
                })
                .collect(),
        });
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let toggle = cx
        .debug_bounds("focused-agent-info-toggle")
        .expect("compact environment button");
    assert!(cx.debug_bounds("agent-changes").is_none());
    assert!(cx.debug_bounds("agent-info-toggle").is_none());
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let changes = cx.debug_bounds("agent-changes").unwrap();
    let agents = cx.debug_bounds("active-agents").unwrap();
    assert!(agents.top() > changes.bottom());
    let worker = cx.debug_bounds("active-agent-Scanning").unwrap();
    let avatar = cx.debug_bounds("active-agent-avatar").unwrap();
    let loader = cx.debug_bounds("active-agent-loader").unwrap();
    assert!(avatar.right() < worker.center().x && loader.left() > worker.center().x);
    let loader_colors = |cx: &mut VisualTestContext| {
        cx.update(|window, _| {
            let scale = window.scale_factor();
            let bounds = loader.scale(scale);
            window.painted_quads().into_iter()
                .filter(|quad| quad.bounds.size.width == px(4.0).scale(scale)
                    && bounds.contains(&quad.bounds.center()))
                .map(|quad| quad.background)
                .collect::<Vec<_>>()
        })
    };
    let before = loader_colors(&mut cx);
    assert_eq!(before.len(), 9);
    cx.background_executor
        .advance_clock(std::time::Duration::from_millis(2765));
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    let after = loader_colors(&mut cx);
    assert_eq!(after.len(), 9);
    assert_ne!(before, after, "loader must keep animating after the old one-shot ends");
    assert!(cx.debug_bounds("active-agent-Reviewed").is_none());
    assert!(cx.debug_bounds("Open in Zed").is_some());
    let primary = cx.debug_bounds("open-default-editor").unwrap();
    let chevron = cx.debug_bounds("open-editor-picker").unwrap();
    assert!(primary.right() <= chevron.left());
    cx.simulate_click(chevron.center(), Modifiers::none());
    assert!(state.read_with(&cx, |s, _| s.editor_picker_open.is_some()));
    cx.simulate_click(chevron.center(), Modifiers::none());
    assert!(state.read_with(&cx, |s, _| s.editor_picker_open.is_none()));
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-changes").is_none());
    assert!(cx.debug_bounds("active-agents").is_none());
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let active_agent = cx.debug_bounds("active-agent-Scanning").unwrap();
    cx.simulate_click(active_agent.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("subagent-back").is_some());
}

#[test]
fn focused_chat_scrolls_from_both_side_margins() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.agent_layout = true;
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.active = true;
        for index in 0..50 {
            chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::User,
                format!("Message {index}"),
            )));
        }
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let chat_bounds = cx.debug_bounds("agent-chat").unwrap();
    let y = chat_bounds.top() + chat_bounds.size.height / 2.0;
    let scroll_top = |cx: &VisualTestContext| {
        state.read_with(cx, |s, _| {
            s.projects[0].sessions[0].panes[0]
                .chat
                .list_state
                .logical_scroll_top()
                .item_ix
        })
    };
    for x in [chat_bounds.left() + px(8.0), chat_bounds.right() - px(30.0)] {
        let previous = scroll_top(&cx);
        cx.simulate_event(gpui::ScrollEvent {
            position: point(x, y),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(300.0))),
            ..Default::default()
        });
        cx.draw_frame().unwrap();
        assert!(
            scroll_top(&cx) < previous,
            "wheel events in each margin scroll the chat"
        );
    }
    let button = cx
        .debug_bounds("scroll-to-bottom-btn")
        .expect("scroll to bottom button");
    assert_eq!(button.size.width, px(32.0));
    assert_eq!(button.center().x, chat_bounds.center().x);
    cx.simulate_click(button.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("scroll-to-bottom-btn").is_none());

    for resize in [false, true] {
        if resize {
            cx.simulate_resize(gpui::size(px(1050.0), px(800.0)));
            cx.draw_frame().unwrap();
        }
        let track = cx.debug_bounds("focused-chat-scrollbar-track").unwrap();
        cx.simulate_click(
            point(track.center().x, track.top() + px(2.0)),
            Modifiers::none(),
        );
        cx.draw_frame().unwrap();
        assert_eq!(
            scroll_top(&cx),
            0,
            "track must reach unmeasured rows (after resize: {resize})"
        );
        assert!(cx.debug_bounds("scroll-to-bottom-btn").is_some());

        cx.simulate_click(
            point(track.center().x, track.bottom() - px(2.0)),
            Modifiers::none(),
        );
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("scroll-to-bottom-btn").is_none());
    }

    let track = cx.debug_bounds("focused-chat-scrollbar-track").unwrap();
    let thumb = |cx: &mut VisualTestContext| {
        cx.update(|window, _| {
            let scale = window.scale_factor();
            let track = track.scale(scale);
            window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.bounds.size.width == px(7.0).scale(scale)
                        && track.contains(&quad.bounds.center())
                })
                .expect("chat scrollbar should have a 7 px thumb")
                .bounds
        })
    };
    let initial_thumb = thumb(&mut cx);
    let scale = cx.update(|window, _| window.scale_factor());
    let grab = point(
        track.center().x,
        px(initial_thumb.center().y.0 / scale),
    );
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    let mut previous = scroll_top(&cx);
    for fraction in [0.75, 0.6, 0.45] {
        cx.simulate_mouse_move(
            point(grab.x, track.top() + track.size.height * fraction),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.draw_frame().unwrap();
        let current = scroll_top(&cx);
        assert!(
            current > 0 && current < previous,
            "drag should stay in history and move monotonically"
        );
        previous = current;
        let height = thumb(&mut cx).size.height.0;
        assert!(
            (height - initial_thumb.size.height.0).abs() <= 1.0,
            "thumb height changed beyond device-pixel rounding"
        );
    }
    cx.simulate_mouse_up(
        point(grab.x, track.top()),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.draw_frame().unwrap();
    assert_eq!(
        scroll_top(&cx),
        0,
        "release must use the final pointer position"
    );
}

#[test]
fn focused_layout_places_usage_above_the_composer() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.agent_layout = true;
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.active = true;
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "Hello".into())));
        chat.set_usage(embedded::AgentUsage {
            input: 100,
            output: 20,
            ..Default::default()
        });
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let usage = cx.debug_bounds("agent-usage").expect("usage visible");
    let composer = cx.debug_bounds("shob-input-bar").unwrap();
    assert!(cx.debug_bounds("input-mode-terminal-pill").is_none());
    assert!(cx.debug_bounds("input-mode-agent-pill").is_none());
    let chat = cx.debug_bounds("agent-chat").unwrap();
    let first_row = cx.debug_bounds("agent-chat-row").unwrap();
    assert_eq!(first_row.top(), chat.top() + px(8.0), "short transcripts start at the top");
    let track = cx.debug_bounds("focused-chat-scrollbar-track").unwrap();
    assert!(track.bottom() >= composer.bottom());
    assert_eq!(track.right(), chat.right());
    let row = cx.debug_bounds("agent-chat-row").unwrap();
    assert_eq!(composer.left(), row.left() + px(16.0));
    assert_eq!(composer.right(), row.right() - px(16.0));
    assert!(usage.bottom() <= chat.top());
    assert!(usage.bottom() < composer.top());
    assert!(chat.size.width > composer.size.width);
}

#[test]
fn usage_composer_accumulates_finalized_responses_and_preserves_unknown_cost() {
    use agent_bridge::AgentEvent;
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.agent_layout = true;
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.active = true;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("agent-usage").is_none(),
        "usage is hidden before a response"
    );
    let empty_bar_height = cx.debug_bounds("shob-input-bar").unwrap().size.height;
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.observe_status(&AgentEvent::Usage(embedded::AgentUsage::default()));
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("agent-usage").is_none(),
        "empty usage remains hidden"
    );
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        // Start a fresh total so the test below can verify known cost accumulation.
        chat.usage = None;
        chat.observe_status(&AgentEvent::Usage(embedded::AgentUsage {
            input: 1200,
            output: 80,
            cache_read: 8000,
            cache_write: 500,
            cost: Some(0.012),
        }));
        chat.observe_status(&AgentEvent::Usage(embedded::AgentUsage {
            input: 300,
            output: 20,
            cache_read: 1000,
            cache_write: 0,
            cost: Some(0.003),
        }));
        chat.observe_status(&AgentEvent::Done);
        assert_eq!(
            chat.usage.unwrap(),
            embedded::AgentUsage {
                input: 1500,
                output: 100,
                cache_read: 9000,
                cache_write: 500,
                cost: Some(0.015),
            }
        );
        cx.notify();
    });
    cx.draw_frame().expect("render usage footer");
    let footer = cx.debug_bounds("agent-usage").expect("agent usage visible");
    let editor = cx.debug_bounds("agent-composer-input").unwrap();
    let bar = cx.debug_bounds("shob-input-bar").unwrap();
    assert_eq!(
        empty_bar_height, bar.size.height,
        "usage must not change input height"
    );
    assert!(footer.bottom() < editor.top());
    if let Some(path) = std::env::var_os("SHOB_USAGE_SCREENSHOT") {
        cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
    }
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.observe_status(&AgentEvent::Usage(embedded::AgentUsage {
            input: 5,
            output: 1,
            ..Default::default()
        }));
        assert_eq!(chat.usage.unwrap().input, 1505);
        assert_eq!(
            chat.usage.unwrap().cost,
            None,
            "partial pricing must not understate cost"
        );
        chat.active = false;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("agent-usage").is_none(),
        "terminal mode hides agent metrics"
    );
}

#[test]
fn agent_changes_indicator_opens_the_reusable_changes_pane() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        state.settings.agent_layout = true;
        let pane = state.pane_mut(1).unwrap();
        pane.chat.active = true;
        pane.chat.info_open_compact = true;
        pane.chat
            .messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "Hello".into())));
        let path = state.projects[0].path.clone();
        state.project_change_counts.insert(
            path,
            Some(project_diff::ChangeSummary {
                files: 3,
                additions: 12,
                deletions: 4,
            }),
        );
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let button = cx.debug_bounds("agent-changes").expect("changes indicator");
    assert!(cx.debug_bounds("agent-changes-added").is_some());
    assert!(cx.debug_bounds("agent-changes-removed").is_some());
    cx.simulate_click(button.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    state.read_with(&cx, |state, _| {
        let session = &state.projects[0].sessions[0];
        assert_eq!(session.panes.len(), 2);
        assert!(
            session
                .panes
                .iter()
                .any(|pane| pane.preview_owner == Some(1) && pane.diff_preview.is_some())
        );
    });
}

#[test]
fn split_view_keeps_legacy_chat_and_shell_drafts_separate() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        let mut workspace = s.saved_workspace(cx);
        let pane = &mut workspace.projects[0].sessions[0].panes[0];
        pane.chat_active = true;
        pane.draft = "echo shell".into();
        pane.agent_draft = "Agent draft".into();
        s.startup_loaded = false;
        s.settings.agent_layout = false;
        s.settings.restore.agent_history = false;
        s.apply_saved_workspace(
            RestoreData {
                workspace,
                scrollbacks: HashMap::new(),
                transcripts: HashMap::new(),
                usage: HashMap::new(),
            },
            cx,
        );
        s.ensure_panes_input_state(window, cx);
        s.create_new_chat(window, cx);
        assert_eq!(s.active_project().unwrap().sessions.len(), 1);
    });
    for one in [false, true, false, true] {
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("sidebar-new-thread").is_some());
        assert_eq!(cx.debug_bounds("agent-attach-file").is_some(), one);
        assert!(cx.debug_bounds("input-mode-agent-pill").is_none());
        state.read_with(&cx, |s, cx| {
            let pane = s.pane_ref(s.focused_pane_id.unwrap()).unwrap();
            assert_eq!(pane.chat.active, one);
            assert_eq!(
                pane.input_state.as_ref().unwrap().read(cx).value().as_str(),
                if one { "Agent draft" } else { "echo shell" },
            );
        });
        let toggle = cx.debug_bounds("layout-switch").unwrap();
        cx.simulate_click(toggle.center(), Modifiers::none());
    }
}

#[test]
fn sidebar_show_more_reveals_the_rest_of_the_active_threads() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_theme_preference(ThemePreference::Light, Some(window), cx);
        for idx in 2..=17 {
            let title: SharedString = format!("Session {idx:02}: conversation title").into();
            s.projects[0].sessions.push(Session {
                id: idx,
                uid: paths::uid("s"),
                scratch_dir: None,
                title: title.clone(),
                display_title: title,
                logo: logos::default_logo(),
                pane_tree: None,
                maximized_pane_id: None,
                panes: Vec::new(),
                status: SessionStatus::Idle,
            });
        }
        s.rebuild_session_index();
        cx.notify();
    });
    cx.simulate_resize(size(px(1200.0), px(900.0)));
    cx.simulate_mouse_move(point(px(600.0), px(400.0)), None, Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    let more = cx.debug_bounds("sidebar-active-show-more").unwrap();
    assert!(cx.debug_bounds("session-row-2").is_none());
    cx.simulate_click(more.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let groups = s.sidebar_thread_groups();
        assert_eq!(groups.active.len(), 17);
        assert_eq!(groups.hidden_active, 0);
    });
    cx.simulate_mouse_move(more.center(), None, Modifiers::none());
    cx.simulate_event(gpui::ScrollEvent {
        position: more.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-3000.0))),
        ..Default::default()
    });
    cx.draw_frame().unwrap();
    let less = cx.debug_bounds("sidebar-active-show-more").unwrap();
    cx.simulate_click(less.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("session-row-2").is_none());
}

#[test]
fn empty_closed_sidebar_search_does_not_invalidate_the_root() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_commands();
    let before = APP_ROOT_RENDERS.with(|count| count.get());
    cx.update_window_entity(&state, |state, window, cx| state.close_sidebar_search(window, cx));
    cx.draw_commands();
    assert_eq!(APP_ROOT_RENDERS.with(|count| count.get()), before);
}
