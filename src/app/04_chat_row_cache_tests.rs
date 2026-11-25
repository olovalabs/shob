#[cfg(test)]
mod chat_row_cache_tests {
    use super::*;
    use agent_bridge::AgentEvent;

    fn apply(chat: &mut PaneChat, event: AgentEvent) {
        chat.observe_status(&event);
        match event {
            AgentEvent::Done | AgentEvent::Closed => {
                finish_chat_tools(&mut chat.messages);
                finish_chat_streaming(&mut chat.messages);
            }
            event => push_chat_event(&mut chat.messages, event),
        }
    }

    fn build(chat: &PaneChat) -> Rc<std::cell::RefCell<ChatRowCache>> {
        AppState::build_chat_rows(chat, chat.running)
    }

    #[test]
    fn completed_turn_disclosure_hides_progress_and_keeps_final_answer() {
        let mut chat = PaneChat { running: true, ..Default::default() };
        let mut user = ChatMessage::new(ChatRole::User, "prompt".into());
        user.source_timestamp = Some(1_000);
        chat.messages.push(Arc::new(user));
        apply(&mut chat, AgentEvent::Text("progress".into()));
        apply(&mut chat, AgentEvent::ToolStart { id: Some("read".into()), name: "read".into(), input: "{}".into() });
        apply(&mut chat, AgentEvent::ToolEnd { id: Some("read".into()), name: "read".into(), output: "contents".into(), failed: false });
        apply(&mut chat, AgentEvent::Text("final answer".into()));
        let cache = build(&chat);
        assert!(cache.borrow().rows.iter().any(|row| matches!(row, ChatRow::Message { message, .. } if message.text == "progress")));
        finish_worked_turn(&mut chat.messages, Some(10_000));
        apply(&mut chat, AgentEvent::Done);
        build(&chat);
        assert_eq!(cache.borrow().rows.len(), 3);
        assert!(matches!(&cache.borrow().rows[1], ChatRow::Worked { open: false, has_details: true, elapsed_ms: Some(9_000), .. }));
        assert!(matches!(&cache.borrow().rows[2], ChatRow::Message { index: 3, message, .. } if message.text == "final answer"));
        chat.expanded_turns.entry(chat.conversation_key.clone()).or_default().insert(1);
        chat.invalidate_message_row(1);
        build(&chat);
        assert_eq!(cache.borrow().rows.len(), 5);
        assert!(matches!(&cache.borrow().rows[2], ChatRow::Message { message, .. } if message.text == "progress"));
        assert!(matches!(&cache.borrow().rows[3], ChatRow::ToolGroup { .. }));
        let built = cache.borrow().built_rows;
        build(&chat);
        assert_eq!(cache.borrow().built_rows, built);
        chat.expanded_turns.get_mut(&chat.conversation_key).unwrap().remove(&1);
        chat.invalidate_message_row(1);
        build(&chat);
        assert_eq!(cache.borrow().rows.len(), 3);
        assert_eq!(chat.messages[1].text, "progress");
        chat.conversation_key = "another-conversation".into();
        assert!(matches!(&build(&chat).borrow().rows[1], ChatRow::Worked { open: false, .. }));
    }

    #[test]
    fn unchanged_frames_and_bursty_deltas_reuse_history_without_cloning_live_text() {
        let mut chat = PaneChat::default();
        for _ in 0..200 {
            chat.messages
                .push(Arc::new(ChatMessage::new(ChatRole::User, "prompt".into())));
            chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                "answer".into(),
            )));
        }
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "next".into())));
        chat.running = true;
        apply(&mut chat, AgentEvent::Text("streaming ".repeat(100)));
        let snapshot = build(&chat);
        let built = snapshot.borrow().built_rows;
        let activity = snapshot.borrow().activity.clone().unwrap();
        build(&chat);
        assert_eq!(snapshot.borrow().built_rows, built);
        let last = chat.messages.len() - 1;
        let address = Arc::as_ptr(&chat.messages[last]);
        for _ in 0..8 {
            let event = AgentEvent::Text("more ".into());
            chat.observe_status(&event);
            assert_eq!(Arc::strong_count(&chat.messages[last]), 1);
            push_chat_event(&mut chat.messages, event);
            assert_eq!(Arc::as_ptr(&chat.messages[last]), address);
        }
        build(&chat);
        let cache = snapshot.borrow();
        assert_eq!(cache.built_rows - built, 2);
        assert_eq!(cache.activity.as_ref(), Some(&activity));
        assert!(matches!(&cache.rows[cache.rows.len() - 2],
            ChatRow::Message { message, .. } if message.text.ends_with("more more more more more more more more ")));
    }

    #[test]
    fn parallel_tools_folding_and_running_transitions_refresh_cached_rows() {
        let mut chat = PaneChat {
            running: true,
            ..Default::default()
        };
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "prompt".into())));
        apply(&mut chat, AgentEvent::Text("first".into()));
        for id in ["one", "two"] {
            apply(
                &mut chat,
                AgentEvent::ToolStart {
                    id: Some(id.into()),
                    name: "read".into(),
                    input: "{}".into(),
                },
            );
        }
        apply(&mut chat, AgentEvent::Text("answer".into()));
        let snapshot = build(&chat);
        apply(
            &mut chat,
            AgentEvent::ToolEnd {
                id: Some("one".into()),
                name: "read".into(),
                output: "contents".into(),
                failed: false,
            },
        );
        build(&chat);
        assert!(matches!(&snapshot.borrow().rows[2],
            ChatRow::ToolGroup { messages, live: true, open: true, .. }
                if messages[0].tool.as_ref().unwrap().output.as_ref() == "contents"
                    && messages[1].tool.as_ref().unwrap().state == ToolState::Running));
        apply(&mut chat, AgentEvent::Done);
        build(&chat);
        {
            let cache = snapshot.borrow();
            assert!(matches!(
                &cache.rows[2],
                ChatRow::ToolGroup {
                    live: false,
                    open: false,
                    ..
                }
            ));
            assert!(matches!(
                &cache.rows[3],
                ChatRow::Message {
                    live: false,
                    copy_range: Some((1, 4)),
                    ..
                }
            ));
            assert!(matches!(
                &cache.rows[1],
                ChatRow::Message {
                    copy_range: None,
                    ..
                }
            ));
            assert!(
                !cache
                    .rows
                    .iter()
                    .any(|row| matches!(row, ChatRow::Running { .. }))
            );
        }
        chat.expanded_tool_groups
            .entry(chat.conversation_key.clone())
            .or_default()
            .insert(2);
        chat.invalidate_message_row(2);
        build(&chat);
        assert!(matches!(
            &snapshot.borrow().rows[2],
            ChatRow::ToolGroup { open: true, .. }
        ));
        let messages = chat.visible_messages_mut();
        Arc::make_mut(&mut messages[2])
            .tool
            .as_mut()
            .unwrap()
            .expanded = true;
        chat.invalidate_message_row(2);
        build(&chat);
        assert!(
            matches!(&snapshot.borrow().rows[2], ChatRow::ToolGroup { messages, .. }
            if messages[0].tool.as_ref().unwrap().expanded)
        );
        chat.running = true;
        build(&chat);
        assert!(matches!(
            &snapshot.borrow().rows[3],
            ChatRow::Message {
                copy_range: None,
                ..
            }
        ));
        chat.running = false;
        build(&chat);
        assert!(matches!(
            &snapshot.borrow().rows[3],
            ChatRow::Message {
                copy_range: Some((1, 4)),
                ..
            }
        ));
    }

    #[test]
    fn cross_turn_worker_updates_refresh_the_original_deduplicated_row() {
        let mut chat = PaneChat::default();
        let mut worker = subagent_chat::Worker {
            id: "worker".into(),
            title: "FirstTitle".into(),
            status: "running".into(),
            transcript: Some(PathBuf::from("/sessions/worker.jsonl")),
            result: None,
        };
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "first".into())));
        apply(
            &mut chat,
            AgentEvent::Subagents {
                tool_id: "first".into(),
                workers: vec![worker.clone()],
            },
        );
        apply(&mut chat, AgentEvent::Text("response".into()));
        let snapshot = build(&chat);
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "second".into())));
        worker.status = "completed".into();
        worker.title = "UpdatedTitle".into();
        apply(
            &mut chat,
            AgentEvent::Subagents {
                tool_id: "second".into(),
                workers: vec![worker.clone()],
            },
        );
        build(&chat);
        {
            let cache = snapshot.borrow();
            assert!(matches!(&cache.rows[1], ChatRow::Subagents { rows, .. }
                if rows.len() == 1 && rows[0].title == "Updated Title" && rows[0].status == "finished"));
            assert_eq!(
                cache
                    .rows
                    .iter()
                    .filter(|row| matches!(row, ChatRow::Subagents { .. }))
                    .count(),
                1
            );
        }
        worker.status = "failed".into();
        apply(
            &mut chat,
            AgentEvent::Subagents {
                tool_id: "first".into(),
                workers: vec![worker],
            },
        );
        build(&chat);
        // The later completed report still wins, exactly as subagent_chat::rows specifies.
        assert!(
            matches!(&snapshot.borrow().rows[1], ChatRow::Subagents { rows, .. }
            if rows[0].status == "finished")
        );
    }

    #[test]
    fn restores_switches_and_panes_never_reuse_stale_content_or_folding() {
        let mut chat = PaneChat::default();
        apply(&mut chat, AgentEvent::Text("old".into()));
        let snapshot = build(&chat);
        *chat.visible_messages_mut() = vec![Arc::new(ChatMessage::new(
            ChatRole::Assistant,
            "restored".into(),
        ))];
        build(&chat);
        assert!(
            matches!(&snapshot.borrow().rows[0], ChatRow::Message { message, .. }
            if message.text == "restored")
        );
        let original = chat.conversation_key.clone();
        chat.expanded_tool_groups
            .entry(original.clone())
            .or_default()
            .insert(0);
        chat.conversation_key = "another".into();
        *chat.visible_messages_mut() = Vec::new();
        apply(
            &mut chat,
            AgentEvent::ToolEnd {
                id: Some("tool".into()),
                name: "read".into(),
                output: "new".into(),
                failed: false,
            },
        );
        build(&chat);
        assert!(matches!(
            &snapshot.borrow().rows[0],
            ChatRow::ToolGroup { open: false, .. }
        ));
        chat.conversation_key = original;
        build(&chat);
        assert!(matches!(
            &snapshot.borrow().rows[0],
            ChatRow::ToolGroup { open: true, .. }
        ));
        let other = PaneChat::default();
        let other_rows = build(&other);
        assert!(!Rc::ptr_eq(&snapshot, &other_rows));
        assert!(other_rows.borrow().rows.is_empty());
        chat.visible_messages_mut().clear();
        build(&chat);
        assert!(snapshot.borrow().rows.is_empty());
    }
}

fn worker_status_label(label: &str) -> &'static str {
    match label {
        "Starting" => "is starting",
        "Running" => "is working",
        "Completed" => "finished",
        "Failed" => "failed",
        "Cancelled" => "was cancelled",
        "Idle" => "is idle",
        _ => "reported",
    }
}

fn chat_row_contains(row: &ChatRow, message_index: usize) -> bool {
    match row {
        ChatRow::Worked { start, .. } => *start == message_index,
        ChatRow::Subagents { start, .. } => *start == message_index,
        ChatRow::ToolGroup {
            start, messages, ..
        } => (*start..*start + messages.len()).contains(&message_index),
        ChatRow::Message { index, .. } => *index == message_index,
        ChatRow::Running { .. } => false,
    }
}

/// Describe only activity supported by events from the current turn.
fn chat_activity(messages: &[Arc<ChatMessage>]) -> String {
    let mut turn = messages
        .iter()
        .rev()
        .take_while(|message| message.role != ChatRole::User);
    let mut active = turn.clone().filter_map(|message| {
        message
            .tool
            .as_ref()
            .filter(|tool| tool.state == ToolState::Running)
    });
    if let Some(tool) = active.next() {
        let label = if chat_batch::is_batch(&tool.name) {
            chat_batch::title(tool)
        } else {
            tool_activity(&tool.name).to_owned()
        };
        let detail = if matches!(
            tool.name.to_ascii_lowercase().as_str(),
            "read"
                | "read_file"
                | "edit"
                | "write"
                | "write_file"
                | "create_file"
                | "str_replace"
                | "str_replace_editor"
                | "patch"
                | "apply_patch"
        ) {
            tool.display
                .activity_detail
                .as_deref()
                .unwrap_or(&tool.display.view.subtitle)
        } else {
            &tool.display.view.subtitle
        };
        let detail = first_line(detail);
        let detail = clip_tool_text(detail, 80).0;
        let extra = active.count();
        let mut activity = label.trim_end_matches('…').to_owned();
        if let Some(count) = chat_batch::known_count(tool) {
            activity.push_str(" · ");
            activity.push_str(&chat_batch::call_count_label(count));
        } else if !detail.is_empty() {
            activity.push_str(" · ");
            activity.push_str(&detail);
        } else if label == "Running a tool…" {
            activity.push_str(" · ");
            activity.push_str(&clip_tool_text(&tool.name, 40).0);
        }
        if extra > 0 {
            activity.push_str(&format!(" (+{extra} active)"));
        }
        activity.push('…');
        return activity;
    }
    match turn.next() {
        Some(message) if message.streaming && message.role == ChatRole::Thinking => "Thinking…",
        Some(message) if message.streaming && message.role == ChatRole::Assistant => {
            "Writing a response…"
        }
        _ => "Working…",
    }
    .to_owned()
}

fn chat_activity_tooltip(messages: &[Arc<ChatMessage>]) -> String {
    let tools: Vec<_> = messages
        .iter()
        .rev()
        .take_while(|message| message.role != ChatRole::User)
        .filter_map(|message| {
            message
                .tool
                .as_ref()
                .filter(|tool| tool.state == ToolState::Running)
        })
        .collect();
    if tools.is_empty() {
        return chat_activity(messages);
    }
    tools
        .iter()
        .take(10)
        .map(|tool| {
            if chat_batch::is_batch(&tool.name) {
                return chat_batch::activity_details(tool);
            }
            let detail = tool
                .display
                .activity_detail
                .as_deref()
                .filter(|_| {
                    matches!(
                        tool_activity(&tool.name),
                        "Reading files…" | "Editing files…"
                    )
                })
                .unwrap_or(&tool.display.view.subtitle);
            let detail = clip_tool_text(first_line(detail), 300).0;
            if detail.is_empty() {
                format!(
                    "{} · {}",
                    tool_activity(&tool.name).trim_end_matches('…'),
                    tool.name
                )
            } else {
                format!(
                    "{} · {}",
                    tool_activity(&tool.name).trim_end_matches('…'),
                    detail
                )
            }
        })
        .chain((tools.len() > 10).then(|| format!("And {} more active tools", tools.len() - 10)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_activity(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "batch" => "Running tools…",
        "read" | "read_file" | "cat" => "Reading files…",
        "edit" | "write" | "write_file" | "create_file" | "str_replace" | "str_replace_editor"
        | "patch" | "apply_patch" => "Editing files…",
        "grep" | "glob" | "ast_grep" | "search" | "rg" | "find" | "list" | "ls" | "list_dir" => {
            "Searching files…"
        }
        "bash" | "shell" | "sh" | "command" | "execute" | "exec" | "eval" | "run"
        | "run_command" | "terminal" => "Running commands…",
        "web_search" | "websearch" | "search_web" | "web_fetch" | "webfetch" | "fetch"
        | "browser" => "Browsing the web…",
        "task" | "agent" | "subagent" | "delegate" | "spawn_agent" | "spawn_subagent" => {
            "Working with subagents…"
        }
        _ => "Running a tool…",
    }
}

#[cfg(test)]
mod chat_activity_tests {
    use super::*;

    #[test]
    fn activity_tracks_current_turn_and_parallel_tools() {
        use agent_bridge::AgentEvent;
        let mut messages = vec![Arc::new(ChatMessage::new(ChatRole::User, "hello".into()))];
        assert_eq!(chat_activity(&messages), "Working…");
        push_chat_event(&mut messages, AgentEvent::Thinking("hmm".into()));
        assert_eq!(chat_activity(&messages), "Thinking…");
        push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some("1".into()),
                name: "read".into(),
                input: r#"{"path":"src/main.rs"}"#.into(),
            },
        );
        assert_eq!(chat_activity(&messages), "Reading files · src/main.rs…");
        push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some("2".into()),
                name: "edit".into(),
                input: r#"{"path":"src/theme.rs"}"#.into(),
            },
        );
        assert_eq!(
            chat_activity(&messages),
            "Editing files · src/theme.rs (+1 active)…"
        );
        assert_eq!(
            chat_activity_tooltip(&messages),
            "Editing files · src/theme.rs\nReading files · src/main.rs"
        );
        push_chat_event(
            &mut messages,
            AgentEvent::ToolEnd {
                id: Some("1".into()),
                name: "read".into(),
                output: String::new(),
                failed: false,
            },
        );
        assert_eq!(chat_activity(&messages), "Editing files · src/theme.rs…");
        messages.push(Arc::new(ChatMessage::new(ChatRole::User, "next".into())));
        assert_eq!(chat_activity(&messages), "Working…");
        push_chat_event(&mut messages, AgentEvent::Text("answer".into()));
        assert_eq!(chat_activity(&messages), "Writing a response…");
    }
}

fn supported_thinking_effort(level: &str) -> bool {
    matches!(
        level,
        "auto" | "inherit" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

fn reasoning_effort_label(level: &str) -> &str {
    match level {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Light",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Maximum",
        _ => level,
    }
}

#[derive(Default)]
struct AgentChecklist {
    list: Option<embedded::task_list::TaskList>,
    dismissal: Option<gpui::Task<()>>,
    scroll: gpui::ScrollHandle,
    browsing: bool,
}

struct PaneChat {
    /// Retained render boundary: token delivery never invalidates the app shell.
    view: std::cell::RefCell<Option<Entity<AgentChatView>>>,
    focused_scrollbar: std::cell::RefCell<Option<Entity<FocusedChatScrollbar>>>,
    view_visible: std::cell::Cell<bool>,
    worker_view: Option<WorkerView>,
    active: bool,
    info_open: Option<bool>,
    info_open_compact: bool,
    bridge: Option<ModelBridge>,
    bridge_model: Option<agent_auth::AgentModel>,

    bridge_resume: Option<String>,
    bridge_needs_context: bool,
    cwd: Option<PathBuf>,
    messages: Vec<Arc<ChatMessage>>,
    running: bool,
    checklist: AgentChecklist,
    requests: std::collections::VecDeque<agent_interaction::Request>,
    follow_ups: std::collections::VecDeque<String>,
    follow_ups_paused: bool,
    follow_ups_collapsed: bool,
    recovery: Option<String>,
    retry_prompt: Option<String>,
    workspace_override: Option<PathBuf>,
    /// Native agent lifecycle, independent of the pane's hidden shell.
    status: SessionStatus,
    /// Completed turns in this conversation.
    completed_turns: u64,
    usage: Option<embedded::AgentUsage>,
    usage_label: SharedString,
    usage_detail: SharedString,
    terminal_draft: String,
    agent_draft: String,
    /// Virtualized transcript state. Only rows near the viewport are built.
    list_state: ListState,
    list_rows: std::cell::Cell<usize>,
    list_key: std::cell::Cell<u64>,
    row_cache: std::rc::Rc<std::cell::RefCell<ChatRowCache>>,
    active_dirty: std::cell::Cell<bool>,
    activity_dirty: std::cell::Cell<bool>,
    dirty_messages: std::cell::RefCell<HashSet<usize>>,
    scroll_handler_installed: std::cell::Cell<bool>,
    auth_refresh_pending: bool,
    auth: Option<AuthModal>,
    selected_model: Option<agent_auth::AgentModel>,
    permission_mode: agent_bridge::PermissionMode,
    first_response_provider: Option<String>,
    /// Retained for compatibility with older saved workspaces.
    model_explicit: bool,
    agent_title: Option<String>,
    prompt_title: Option<String>,
    conversation_key: String,
    legacy_models: Vec<workspace_state::SavedModelChat>,
    fast_mode: bool,
    thinking_override: Option<String>,
    thinking_by_model: HashMap<String, String>,
    pub autoscroll: bool,
    expanded_tool_groups: HashMap<String, HashSet<usize>>,
    // Restored completed turns start collapsed; expansion is transient view state.
    expanded_turns: HashMap<String, HashSet<usize>>,
    shob_session_id: Option<String>,
    conversation: Option<conversations::Log>,
    startup_error: Option<String>,
}

impl Default for PaneChat {
    fn default() -> Self {
        Self {
            view: Default::default(),
            focused_scrollbar: Default::default(),
            view_visible: std::cell::Cell::new(false),
            active: false,
            info_open: None,
            info_open_compact: false,
            worker_view: None,
            bridge: None,
            bridge_model: None,

            bridge_resume: None,
            bridge_needs_context: false,
            cwd: None,
            messages: Vec::new(),
            running: false,
            checklist: AgentChecklist::default(),
            requests: Default::default(),
            follow_ups: Default::default(),
            follow_ups_paused: false,
            follow_ups_collapsed: false,
            recovery: None,
            retry_prompt: None,
            workspace_override: None,
            status: SessionStatus::Idle,
            completed_turns: 0,
            usage: None,
            usage_label: "In — · Out — · Cache R — / W — · Cost —".into(),
            usage_detail: "Conversation usage appears after a model response completes.".into(),
            terminal_draft: String::new(),
            agent_draft: String::new(),
            list_state: {
                let state = ListState::new(0, ListAlignment::Bottom, px(600.0));
                state.set_follow_mode(FollowMode::Tail);
                state
            },
            list_rows: std::cell::Cell::new(0),
            list_key: std::cell::Cell::new(0),
            row_cache: Default::default(),
            active_dirty: std::cell::Cell::new(false),
            activity_dirty: std::cell::Cell::new(false),
            dirty_messages: std::cell::RefCell::new(HashSet::new()),
            scroll_handler_installed: std::cell::Cell::new(false),
            auth_refresh_pending: false,
            auth: None,
            selected_model: None,
            permission_mode: agent_bridge::PermissionMode::Ask,
            first_response_provider: None,
            model_explicit: false,
            agent_title: None,
            prompt_title: None,
            conversation_key: conversations::conversation_key(None),
            legacy_models: Vec::new(),
            fast_mode: true,
            thinking_override: None,
            thinking_by_model: HashMap::new(),
            autoscroll: true,
            expanded_tool_groups: HashMap::new(),
            expanded_turns: HashMap::new(),
            shob_session_id: None,
            conversation: None,
            startup_error: None,
        }
    }
}

/// Renders against the owner only while constructing elements. Event/list
/// callbacks retain weak handles, not an AppState borrow or transcript snapshot.
struct AgentChatView {
    app: gpui::WeakEntity<AppState>,
    pane_id: usize,
    #[cfg(test)]
    renders: usize,
}

impl Render for AgentChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.app
            .update(cx, |app, cx| {
                let Some(pane) = app.pane_ref(self.pane_id) else {
                    return div().into_any_element();
                };
                app.render_agent_chat_body(pane, window, cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}
