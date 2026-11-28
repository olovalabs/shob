#[test]
fn custom_bot_can_be_created_opened_and_edited_after_restore() {
    let scratch = tempfile::tempdir().unwrap();
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        state.scratch_dir = scratch.path().join("scratch");
        cx.notify();
    });

    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("new-session-create-bot").is_none());
    let new_thread = cx.debug_bounds("sidebar-new-thread").unwrap();
    cx.simulate_click(new_thread.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let no_project = cx.debug_bounds("new-session-no-project").unwrap();
    let create_bot = cx.debug_bounds("new-session-create-bot").unwrap();
    assert!((create_bot.top() - no_project.bottom()).abs() <= px(2.0));
    cx.simulate_keystrokes("down down enter");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("custom-bot-form").is_some());
    cx.simulate_keystrokes("escape");
    cx.draw_frame().unwrap();

    cx.simulate_click(new_thread.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    cx.simulate_input("create bot");
    cx.draw_frame().unwrap();
    let create_bot = cx.debug_bounds("new-session-create-bot").unwrap();
    cx.simulate_click(create_bot.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("new-session-project-modal").is_none());
    let name = cx.debug_bounds("custom-bot-name").unwrap();
    let instructions = cx.debug_bounds("custom-bot-instructions").unwrap();
    let create = cx.debug_bounds("custom-bot-save").unwrap();
    cx.simulate_click(create.center(), Modifiers::none());
    assert!(state.read_with(&cx, |state, _| state.bots.is_empty()));
    cx.simulate_click(name.center(), Modifiers::none());
    cx.simulate_input("Product Help");
    cx.draw_frame().unwrap();
    cx.simulate_click(create.center(), Modifiers::none());
    assert!(state.read_with(&cx, |state, _| state.bots.is_empty()));
    cx.simulate_click(instructions.center(), Modifiers::none());
    cx.simulate_input("Answer product questions.");
    cx.draw_frame().unwrap();
    let mint = cx.debug_bounds("custom-bot-palette-Mint").unwrap();
    cx.simulate_click(mint.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let triangle = cx.debug_bounds("custom-bot-shape-Triangle").unwrap();
    cx.simulate_click(triangle.center(), Modifiers::none());
    cx.draw_frame().unwrap();

    let model = cx.debug_bounds("custom-bot-model").unwrap();
    cx.simulate_click(model.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-panel").is_some());
    cx.simulate_keystrokes("escape");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-panel").is_none());
    assert!(cx.debug_bounds("custom-bot-form").is_some());

    if let Some(path) = std::env::var_os("SHOB_BOT_EDITOR_SCREENSHOT") {
        let bounds = cx.debug_bounds("custom-bot-form").unwrap();
        cx.update(|window, _| {
            let image = window.render_to_image().unwrap();
            let scale = image.width() as f32 / f32::from(window.viewport_size().width);
            let pixels = |dimension| (f32::from(dimension) * scale).round() as u32;
            image::imageops::crop_imm(
                &image,
                pixels(bounds.left()),
                pixels(bounds.top()),
                pixels(bounds.size.width),
                pixels(bounds.size.height),
            )
            .to_image()
            .save(path)
            .unwrap();
        });
    }
    cx.simulate_click(create.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("custom-bot-form").is_none());
    cx.background_executor.advance_clock(SIDEBAR_ROW_MOTION);
    cx.draw_frame().unwrap();
    let row = cx.debug_bounds("custom-bot-row").unwrap();
    cx.simulate_click(row.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-conversation-header").is_some());

    let regular = cx.debug_bounds("session-row-1").unwrap();
    let row = cx.debug_bounds("custom-bot-row").unwrap();
    assert!(row.top() < regular.top());
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(0.0), px(10.0)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.draw_frame().unwrap();
    let regular = cx.debug_bounds("session-row-1").unwrap();
    let below = point(
        regular.center().x,
        (regular.center().y + regular.bottom()) / 2.0,
    );
    cx.simulate_mouse_move(below, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::none());
    cx.draw_frame().unwrap();
    cx.background_executor.advance_clock(SIDEBAR_ROW_MOTION);
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("custom-bot-row").unwrap().top()
            > cx.debug_bounds("session-row-1").unwrap().top()
    );

    let row = cx.debug_bounds("custom-bot-row").unwrap();
    cx.simulate_mouse_move(row.center(), None, Modifiers::none());
    cx.draw_frame().unwrap();
    let actions = cx.debug_bounds("custom-bot-actions").unwrap();
    cx.simulate_click(actions.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let rename = cx.debug_bounds("sidebar-thread-rename").unwrap();
    cx.simulate_click(rename.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    cx.simulate_input("Wholesale planning");
    cx.simulate_keystrokes("enter");

    let (id, pane_id, saved, saved_settings) = cx.update_window_entity(&state, |state, _, cx| {
        let active = state.active_session().unwrap();
        let pane = state.one_mode_pane(active).unwrap();
        let bot = state.bot_for_pane(pane.id).unwrap();
        assert_eq!(&*bot.system_prompt, "Answer product questions.");
        assert_eq!(bot.id, active.uid);
        assert_eq!(bot.avatar.palette, bot_avatar::Palette::Mint);
        assert_eq!(bot.avatar.shape, bot_avatar::Shape::Triangle);
        assert!(!state.settings.agent_layout, "bot should not switch global layouts");
        assert_eq!(state.sidebar_thread_title(active).as_str(), "Wholesale planning");
        assert_eq!(bot.name.as_str(), "Product Help");
        (
            bot.id.clone(),
            pane.id,
            state.saved_workspace(cx),
            serde_json::to_string(&state.saved_settings()).unwrap(),
        )
    });
    let workspace = workspace_state::load_from_text(&serde_json::to_string(&saved).unwrap());
    cx.update_window_entity(&state, |state, window, cx| {
        state.startup_loaded = false;
        state.settings = serde_json::from_str(&saved_settings).unwrap();
        state.apply_saved_workspace(
            RestoreData {
                workspace,
                scrollbacks: HashMap::new(),
                transcripts: HashMap::new(),
                usage: HashMap::new(),
            },
            cx,
        );
        state.theme_preference = ThemePreference::Dark;
        state.apply_theme_preference(Some(window), cx);
    });
    cx.draw_frame().unwrap();
    cx.background_executor.advance_clock(SIDEBAR_ROW_MOTION);
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("custom-bot-row").unwrap().top()
            > cx.debug_bounds("session-row-1").unwrap().top()
    );
    state.read_with(&cx, |state, _| {
        let session = state.active_session().unwrap();
        assert_eq!(state.sidebar_thread_title(session).as_str(), "Wholesale planning");
    });
    assert!(cx.debug_bounds("custom-bot-edit").is_none());
    let row = cx.debug_bounds("custom-bot-row").unwrap();
    cx.simulate_mouse_move(row.center(), None, Modifiers::none());
    cx.draw_frame().unwrap();
    let actions = cx.debug_bounds("custom-bot-actions").unwrap();
    if let Some(path) = std::env::var_os("SHOB_BOT_EDITOR_SCREENSHOT") {
        cx.update(|window, _| {
            let image = window.render_to_image().unwrap();
            let scale = image.width() as f32 / f32::from(window.viewport_size().width);
            let width = (f32::from(row.right() + px(12.0)) * scale).ceil() as u32;
            let height = (f32::from(row.bottom() + px(88.0)) * scale).ceil() as u32;
            image::imageops::crop_imm(&image, 0, 0, width, height)
                .to_image()
                .save(PathBuf::from(path).with_extension("hover.png"))
                .unwrap();
        });
    }
    cx.simulate_click(actions.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("custom-bot-form").is_none());
    let edit = cx.debug_bounds("custom-bot-edit").unwrap();
    if let Some(path) = std::env::var_os("SHOB_BOT_EDITOR_SCREENSHOT") {
        cx.update(|window, _| {
            let image = window.render_to_image().unwrap();
            let scale = image.width() as f32 / f32::from(window.viewport_size().width);
            let width = (f32::from(actions.right().max(edit.right()) + px(24.0)) * scale)
                .ceil() as u32;
            let height = (f32::from(edit.bottom() + px(72.0)) * scale).ceil() as u32;
            image::imageops::crop_imm(&image, 0, 0, width, height)
                .to_image()
                .save(PathBuf::from(path).with_extension("menu.png"))
                .unwrap();
        });
    }
    cx.simulate_click(edit.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("custom-bot-form").is_some(),
        "Edit bot menu item at {edit:?} did not open the bot form (row: {:?})",
        cx.debug_bounds("custom-bot-row"),
    );
    cx.update_window_entity(&state, |state, window, cx| {
        let form = state.bot_form.as_ref().unwrap();
        assert_eq!(form.name.read(cx).value().as_str(), "Product Help");
        set_input_text(&form.name, "Catalog Assistant".into(), window, cx);
        set_input_text(
            &form.instructions,
            "Use the product catalogue.".into(),
            window,
            cx,
        );
    });
    let save = cx.debug_bounds("custom-bot-save").unwrap();
    cx.simulate_click(save.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let row = cx.debug_bounds("custom-bot-row").unwrap();
    cx.simulate_click(row.center(), Modifiers::none());
    state.read_with(&cx, |state, _| {
        let session = state.active_session().unwrap();
        assert_eq!(session.uid, id);
        assert_eq!(state.sidebar_thread_title(session).as_str(), "Wholesale planning");
        assert_eq!(
            state.bot_for_pane(pane_id).unwrap().name.as_str(),
            "Catalog Assistant",
        );
        assert_eq!(state.one_mode_pane(session).unwrap().id, pane_id);
        assert_eq!(
            &*state.bot_for_pane(pane_id).unwrap().system_prompt,
            "Use the product catalogue.",
        );
    });
}
