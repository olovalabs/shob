impl AppState {
    fn update_chat_mode(
        pane: &mut TerminalPane,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if pane.tool_panel || pane.chat.active == active {
            return false;
        }
        let Some(input) = pane.input_state.clone() else {
            return false;
        };
        let current = input.read(cx).value().to_string();
        let draft = if active {
            pane.chat.terminal_draft = current;
            std::mem::take(&mut pane.chat.agent_draft)
        } else {
            pane.chat.agent_draft = current;
            std::mem::take(&mut pane.chat.terminal_draft)
        };
        pane.chat.active = active;
        pane.input_browser_mentioned = active && agent_bridge::has_browser_mention(&draft);
        pane.input_prompt_empty = draft.trim().is_empty();
        pane.chat.worker_view = None;
        pane.completion_menu = None;
        pane.history_index = None;
        pane.pending_draft.clear();
        input.update(cx, |state, cx| {
            if active {
                state.clear_highlighter(cx);
            } else {
                state.set_highlighter("bash", cx);
            }
            state.set_placeholder(
                if active {
                    "Message the agent…"
                } else {
                    "Run commands"
                },
                window,
                cx,
            );
        });
        set_input_text(&input, draft, window, cx);
        true
    }
    fn open_agent_auth(
        &mut self,
        pane_id: usize,
        page: AgentDialogPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let cwd = self.agent_working_directory(pane);
        self.focused_pane_id = Some(pane_id);
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Enter API key or credential"));
        let provider_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Other provider ID"));
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if page == AgentDialogPage::Models {
                "Search models..."
            } else {
                "Search providers"
            })
        });
        let focus = cx.focus_handle();
        if page == AgentDialogPage::Auth {
            provider_input.update(cx, |state, cx| state.focus(window, cx));
        } else {
            filter.update(cx, |state, cx| state.focus(window, cx));
        }
        let mut auth = AuthModal {
            page,
            model_filter: ModelPickerFilter::All,
            model_scroll: VirtualListScrollHandle::new(),
            provider_scroll: VirtualListScrollHandle::new(),
            focus,
            cwd,
            methods_discovery: None,
            custom_provider: None,
            provider_operation: None,
            provider_input,
            filter,
            input,
            login: None,
            selected: None,
            output: String::new(),
            error: None,
        };
        self.agent_catalog.refresh();
        if page == AgentDialogPage::Auth {
            auth.discover_auth_methods();
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.permission_menu_open = false;
            pane.chat.auth = Some(auth);
            cx.notify();
        }
    }

    fn close_agent_auth(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.permission_menu_open = false;
            if let Some(mut auth) = pane.chat.auth.take() {
                if let Some(mut login) = auth.login.take() {
                    login.cancel();
                }
                auth.input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        self.focus_pane(pane_id, window, cx);
        cx.notify();
    }

    fn set_agent_dialog_page(
        &mut self,
        pane_id: usize,
        page: AgentDialogPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if page == AgentDialogPage::Models && auth.page != page {
            auth.custom_provider = None;
            if let Some(mut login) = auth.login.take() {
                login.cancel();
            }
            auth.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            auth.selected = None;
            auth.output.clear();
            auth.error = None;
        }
        if auth.page != page {
            let placeholder = if page == AgentDialogPage::Models {
                "Search models..."
            } else {
                "Search providers"
            };
            auth.filter.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
                state.set_value("", window, cx);
            });
        }
        if page == AgentDialogPage::Auth && auth.page != page {
            auth.discover_auth_methods();
        }
        auth.page = page;
        if page == AgentDialogPage::Models {
            auth.filter.update(cx, |state, cx| state.focus(window, cx));
        } else {
            auth.provider_input
                .update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn set_permission_mode(&mut self, mode: agent_bridge::PermissionMode, cx: &mut Context<Self>) {
        self.settings.permission_mode = mode;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    pane.chat.permission_mode = mode;
                    pane.permission_menu_open = false;
                    if let Some(bridge) = &pane.chat.bridge {
                        bridge.set_permission_mode(mode);
                    }
                }
            }
        }
        self.save_agent_preferences(cx);
        cx.notify();
    }

    fn select_agent_model(
        &mut self,
        pane_id: usize,
        model: Option<agent_auth::AgentModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let keep_picker_open = pane.chat.active
            && pane.model_picker_bounds.get().size.width > px(0.0)
            && pane
                .chat
                .auth
                .as_ref()
                .is_some_and(|auth| auth.page == AgentDialogPage::Models);
        self.settings.default_agent_model = model.clone();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    pane.chat.startup_error = None;
                    pane.chat.model_explicit = false;
                    pane.chat.selected_model = model.clone();
                }
            }
        }
        self.save_agent_preferences(cx);
        if keep_picker_open {
            cx.notify();
        } else {
            self.close_agent_auth(pane_id, window, cx);
        }
    }

    fn toggle_favorite_model(&mut self, model: &agent_auth::AgentModel, cx: &mut Context<Self>) {
        if let Some(index) = self
            .settings
            .favorite_agent_models
            .iter()
            .position(|favorite| favorite == model)
        {
            self.settings.favorite_agent_models.remove(index);
        } else {
            self.settings.favorite_agent_models.push(model.clone());
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn select_provider_for_auth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }
        let primary_var = agent_auth::key_label(&provider);
        let input_to_focus = auth.input.clone();
        auth.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder(format!("Paste {primary_var} here..."), window, cx);
            state.focus(window, cx);
        });
        auth.selected = Some(provider.clone());
        auth.output.clear();
        auth.error = None;
        cx.notify();
        window.defer(cx, move |window, cx| {
            input_to_focus.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        });
    }

    fn start_agent_oauth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }
        auth.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder("Enter code or response if prompted...", window, cx);
            state.focus(window, cx);
        });
        auth.selected = Some(provider.clone());
        auth.output.clear();
        auth.error = None;
        match agent_auth::Login::start(&provider, &auth.cwd) {
            Ok(login) => auth.login = Some(login),
            Err(error) => auth.error = Some(error),
        }
        cx.notify();
    }

    fn start_agent_auth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.chat.auth.as_ref())
        else {
            return;
        };
        let methods = agent_auth::auth_methods(&provider, &auth.cwd);
        if methods.oauth && !methods.api_key {
            self.start_agent_oauth(pane_id, provider, window, cx);
        } else {
            self.select_provider_for_auth(pane_id, provider, window, cx);
        }
    }

    fn open_custom_provider(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields = [
            "Provider ID (e.g. my-provider)",
            "Provider name",
            "https://example.com/v1",
            "API key",
            "Model IDs, separated by commas",
        ]
        .map(|placeholder| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)));
        fields[3].update(cx, |state, cx| state.set_masked(true, window, cx));
        fields[0].update(cx, |state, cx| state.focus(window, cx));
        if let Some(auth) = self.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut()) {
            if let Some(mut login) = auth.login.take() {
                login.cancel();
            }
            auth.selected = None;
            auth.error = None;
            auth.filter
                .update(cx, |state, cx| state.set_value("", window, cx));
            auth.custom_provider = Some(fields);
        }
        cx.notify();
    }

    fn save_custom_provider(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(auth) = self.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut()) else {
            return;
        };
        if auth.provider_operation.is_some() {
            return;
        }
        let Some(fields) = &auth.custom_provider else {
            return;
        };
        let [id, name, endpoint, key, models] = fields
            .each_ref()
            .map(|field| field.read(cx).value().trim().to_string());
        if !agent_auth::provider_visible_in_chat_ui(&id) {
            auth.error = Some("That provider ID is reserved. Choose a different ID.".into());
            cx.notify();
            return;
        }
        let models = models
            .split([',', '\n'])
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .take(129)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        auth.provider_operation = Some(receiver);
        auth.error = None;
        std::thread::spawn(move || {
            let result =
                agent_auth::add_openai_compatible_provider(&id, &name, &endpoint, &key, &models)
                    .map(|()| None);
            let _ = sender.send(result);
        });
        cx.notify();
    }

    fn submit_agent_auth(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(auth) = pane.chat.auth.as_mut() else {
            return;
        };
        if auth.page != AgentDialogPage::Auth {
            return;
        }
        if auth.custom_provider.is_some() {
            self.save_custom_provider(pane_id, cx);
            return;
        }
        // Enter while searching filters the list; it must not submit a key.
        if auth.filter.read(cx).focus_handle().is_focused(window) {
            return;
        }

        // Forward input to an interactive login while it is active.
        if let Some(login) = auth.login.as_mut() {
            let value = auth.input.read(cx).value().to_string();
            let result = login.submit(&value);
            auth.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            if let Err(error) = result {
                auth.error = Some(error);
            }
            cx.notify();
            return;
        }

        // 2. If a provider is selected and input contains an API key, save it!
        if let Some(provider) = auth.selected.clone() {
            let key = auth.input.read(cx).value().trim().to_string();
            if key.is_empty() {
                auth.error = Some("API key cannot be empty".to_string());
                cx.notify();
                return;
            }

            match agent_auth::save_api_key(&provider, &key) {
                Ok(()) => {
                    pane.chat.auth_refresh_pending = pane.chat.bridge.is_some();
                    auth.error = None;
                    auth.output = format!("Connected to {provider} successfully with API key!");
                    auth.input
                        .update(cx, |state, cx| state.set_value("", window, cx));

                    // Update live readiness in provider list
                    let mut found = false;
                    for p in &mut self.agent_catalog.data.providers {
                        if p.id == provider {
                            p.ready = true;
                            p.source = Some("Stored API key".to_string());
                            found = true;
                        }
                    }
                    if !found {
                        self.agent_catalog.data.providers.push(agent_auth::Provider {
                            name: None,
                            custom: false,
                            id: provider.clone(),
                            ready: true,
                            source: Some("Stored API key".to_string()),
                        });
                    }

                    // Refresh the catalog off the UI thread.
                    self.agent_catalog.invalidate();
                }
                Err(error) => {
                    auth.error = Some(format!("Could not save API key: {error}"));
                }
            }
            cx.notify();
            return;
        }

        // 3. Fallback: manual provider input
        if auth.methods_discovery.is_some() {
            return;
        }
        let provider = auth.provider_input.read(cx).value().trim().to_string();
        if provider.is_empty() {
            return;
        }
        if !agent_auth::provider_visible_in_chat_ui(&provider) {
            auth.error = Some("Local providers are unavailable in Shob agent chat.".into());
            cx.notify();
            return;
        }
        self.start_agent_auth(pane_id, provider, window, cx);
    }

    fn remove_custom_provider(&mut self, pane_id: usize, provider: String, cx: &mut Context<Self>) {
        let Some(auth) = self.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut()) else {
            return;
        };
        if auth.provider_operation.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        auth.provider_operation = Some(receiver);
        std::thread::spawn(move || {
            let _ =
                sender.send(agent_auth::remove_custom_provider(&provider).map(|()| Some(provider)));
        });
        cx.notify();
    }

    fn disconnect_agent_provider(
        &mut self,
        pane_id: usize,
        provider: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(auth) = pane.chat.auth.as_mut() else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }

        match agent_auth::disconnect_provider(&provider) {
            Ok(()) => {
                auth.error = None;
                auth.output = format!("Disconnected from {provider}.");

                // Mark provider not ready
                for p in &mut self.agent_catalog.data.providers {
                    if p.id == provider {
                        p.ready = false;
                        p.source = None;
                    }
                }

                // Refresh providers and models off the UI thread.
                self.agent_catalog.invalidate();
                self.refresh_bridges_for_provider(&provider);
            }
            Err(error) => {
                auth.error = Some(format!("Could not disconnect {provider}: {error}"));
            }
        }
        cx.notify();
    }

    fn refresh_bridges_for_provider(&mut self, provider: &str) {
        for pane in self
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.sessions)
            .flat_map(|session| &mut session.panes)
        {
            if pane.chat.bridge.is_some()
                && pane
                    .chat
                    .bridge_model
                    .as_ref()
                    .is_some_and(|model| model.provider == provider)
            {
                pane.chat.auth_refresh_pending = true;
            }
        }
    }
}
