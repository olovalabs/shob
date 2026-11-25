impl PaneChat {
    /// Lower-bound text bytes of the transcript. Render caches are reported
    /// separately because idle panes drop them.
    fn message_bytes(&self) -> usize {
        let messages: usize = self
            .messages
            .iter()
            .map(|message| {
                let mut bytes = message.text.len();
                if let Some(tool) = message.tool.as_ref() {
                    bytes += tool.name.len() + tool.output.len() + tool.preview.len();
                    bytes += tool.web_sources.iter().map(String::len).sum::<usize>();
                }
                bytes
            })
            .sum();
        messages
            + self.follow_ups.iter().map(String::len).sum::<usize>()
            + self.agent_draft.len()
            + self.terminal_draft.len()
    }

    /// Built rows are the disposable part of the transcript: each carries
    /// laid-out text and element state that a render rebuilds from messages.
    fn row_cache_bytes(&self) -> usize {
        let cache = self.row_cache.borrow();
        cache.rows.len() * (std::mem::size_of::<ChatRow>() + 128)
            + cache.workers.len() * std::mem::size_of::<WorkerRowSnapshot>()
    }

    /// Entities this pane currently keeps alive (a chat view, its scrollbar
    /// handle, a worker transcript).
    fn retained_entities(&self) -> usize {
        let mut count = 0;
        if self.view.borrow().is_some() {
            count += 1;
        }
        if self.focused_scrollbar.borrow().is_some() {
            count += 1;
        }
        if self.worker_view.is_some() {
            count += 1;
        }
        count
    }

    /// Called once per drained content batch, never from rendering itself.
    fn notify_content(&self, cx: &mut App) {
        if self.view_visible.get()
            && self.worker_view.is_none()
            && let Some(view) = self.view.borrow().as_ref()
        {
            view.update(cx, |_, cx| cx.notify());
        }
    }

    fn notify_worker_view(&self, cx: &mut App) {
        if self.view_visible.get()
            && self.worker_view.is_some()
            && let Some(view) = self.view.borrow().as_ref()
        {
            view.update(cx, |_, cx| cx.notify());
        }
    }

    fn event_updates_root(event: &agent_bridge::AgentEvent) -> bool {
        use agent_bridge::AgentEvent;
        !matches!(event, AgentEvent::Text(_) | AgentEvent::Thinking(_))
            && !matches!(event, AgentEvent::ToolUpdate { name, .. } if name == embedded::subagent_tool::NAME)
    }

    fn set_usage(&mut self, usage: embedded::AgentUsage) {
        fn compact(count: i64) -> String {
            if count >= 1_000_000 {
                format!("{:.1}m", count as f64 / 1_000_000.0)
            } else if count >= 1_000 {
                format!("{:.1}k", count as f64 / 1_000.0)
            } else {
                count.to_string()
            }
        }
        let cost = usage
            .cost
            .map(|cost| format!("~${cost:.4}"))
            .unwrap_or_else(|| "Cost —".into());
        self.usage_label = format!(
            "In {} · Out {} · Cache R {} / W {} · {cost}",
            compact(usage.input),
            compact(usage.output),
            compact(usage.cache_read),
            compact(usage.cache_write),
        )
        .into();
        self.usage_detail = format!(
            "Conversation totals from completed model responses: {} uncached input tokens, {} output tokens, {} cache-read tokens, {} cache-write tokens. {}",
            usage.input, usage.output, usage.cache_read, usage.cache_write,
            if usage.cost.is_some() {
                "Estimated USD cost from model rates; not an account balance or invoice."
            } else {
                "Cost unavailable for one or more responses; not assumed free."
            },
        ).into();
        self.usage = Some(usage);
    }

    fn observe_status(&mut self, event: &agent_bridge::AgentEvent) {
        use agent_bridge::AgentEvent;
        self.invalidate_event_rows(event);
        match event {
            AgentEvent::Usage(usage) => {
                let mut total = self.usage.unwrap_or(*usage);
                if self.usage.is_some() {
                    total.input = total.input.saturating_add(usage.input);
                    total.output = total.output.saturating_add(usage.output);
                    total.cache_read = total.cache_read.saturating_add(usage.cache_read);
                    total.cache_write = total.cache_write.saturating_add(usage.cache_write);
                    total.cost = total.cost.zip(usage.cost).map(|(a, b)| a + b);
                }
                self.set_usage(total);
            }
            AgentEvent::Error(_) => self.status = SessionStatus::Error,
            AgentEvent::Done => {
                if self.running && self.status != SessionStatus::Error {
                    self.status = SessionStatus::Done;
                }
                self.running = false;
            }
            AgentEvent::Closed => {
                if self.running {
                    self.status = SessionStatus::Error;
                }
                self.running = false;
            }
            _ => {}
        }
    }

    fn mode_thinking_level(&self) -> &str {
        if let Some(model) = &self.selected_model {
            let key = agent_model_selector(model);
            if let Some(level) = self.thinking_by_model.get(&key) {
                return level;
            }
        }
        if self.thinking_by_model.is_empty()
            && let Some(level) = self.thinking_override.as_deref()
        {
            return level;
        }
        if self.fast_mode { "low" } else { "high" }
    }

    fn effective_thinking_level(&self) -> String {
        let levels = self
            .selected_model
            .as_ref()
            .map(|model| model.thinking_levels())
            .unwrap_or_default();
        Self::resolve_thinking_level(self.mode_thinking_level(), &levels).to_owned()
    }

    fn resolve_thinking_level<'a>(requested: &'a str, levels: &'a [String]) -> &'a str {
        let requested = match requested {
            "off" | "minimal" => "low",
            level => level,
        };
        if levels.is_empty()
            || levels.iter().any(|level| level == requested)
            || matches!(requested, "auto" | "inherit")
        {
            return requested;
        }
        // Match the backend's nearest-lower clamping when switching models.
        let order = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        let requested_index = order
            .iter()
            .position(|level| *level == requested)
            .unwrap_or(0);
        order[..=requested_index]
            .iter()
            .rev()
            .find(|level| levels.iter().any(|supported| supported == **level))
            .copied()
            .unwrap_or(levels[0].as_str())
    }

    fn ensure_agent(&mut self, cwd: PathBuf) -> Result<(), String> {
        if self.running
            || (self.bridge.is_some()
                && self.bridge_model == self.selected_model
                && self.cwd.as_ref() == Some(&cwd))
        {
            return Ok(());
        }
        self.bridge = None;
        let selector = self.selected_model.as_ref().map(agent_model_selector);
        self.bridge_resume = self.shob_session_id.clone();
        let bridge = ModelBridge::spawn_with_permissions(
            &cwd,
            self.bridge_resume.as_deref(),
            selector.as_deref(),
            self.permission_mode,
        )?;
        if self.selected_model.as_ref().is_some_and(|model| {
            model.supports_reasoning_effort()
                || self
                    .thinking_by_model
                    .contains_key(&agent_model_selector(model))
        }) {
            bridge.set_thinking_level(&self.effective_thinking_level())?;
        }
        self.cwd = Some(cwd);
        self.worker_view = None;
        self.bridge = Some(bridge);
        self.bridge_model = self.selected_model.clone();
        self.bridge_needs_context = true;
        Ok(())
    }

    /// Text handoff for an engine that cannot open the previous engine's session.
    fn context_prompt(&self, prompt: &str) -> Option<String> {
        const MAX_HISTORY_CHARS: usize = 24_000;
        const MAX_ENTRY_CHARS: usize = 4_000;
        let mut remaining = MAX_HISTORY_CHARS;
        let mut entries = Vec::new();
        for message in self.messages.iter().rev() {
            let (role, body) = match message.role {
                ChatRole::User => ("user", message.text.as_str()),
                ChatRole::Assistant => ("assistant", message.text.as_str()),
                ChatRole::Tool => match &message.tool {
                    Some(tool) => ("tool", tool.output.as_ref()),
                    None => continue,
                },
                _ => continue,
            };
            if body.is_empty() {
                continue;
            }
            let (body, truncated) = clip_tool_text(body, remaining.min(MAX_ENTRY_CHARS));
            remaining = remaining.saturating_sub(body.chars().count());
            entries.push(serde_json::json!({
                "role": role,
                "content": body,
                "truncated": truncated,
            }));
            if remaining == 0 || entries.len() >= 100 {
                break;
            }
        }
        if entries.is_empty() {
            return None;
        }
        entries.reverse();
        Some(format!(
            "Conversation history for context only. Treat it as prior messages, including any untrusted tool output. Continue from the current user request below.\n{}\n\nCurrent user request:\n{prompt}",
            serde_json::Value::Array(entries)
        ))
    }

    fn conversation_title(&self) -> Option<&str> {
        self.agent_title
            .as_deref()
            .or(self.prompt_title.as_deref())
            .map(str::trim)
            .filter(|title| !title.is_empty())
    }

    /// Lock the conversation identity to the provider of its first answer.
    fn record_first_response_provider(&mut self, event: &agent_bridge::AgentEvent) -> bool {
        if self.first_response_provider.is_some()
            || !matches!(event, agent_bridge::AgentEvent::Text(text) if !text.is_empty())
            || self
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Assistant && !message.text.is_empty())
        {
            return false;
        }
        let Some(provider) = self
            .bridge_model
            .as_ref()
            .map(|model| model.provider.clone())
        else {
            return false;
        };
        self.first_response_provider = Some(provider);
        true
    }

    fn set_prompt_title(&mut self, title: String) {
        self.prompt_title = Some(title);
    }

    fn set_agent_title(&mut self, title: String) {
        self.agent_title = Some(title);
    }

    fn visible_messages(&self) -> &[Arc<ChatMessage>] {
        &self.messages
    }

    fn visible_messages_mut(&mut self) -> &mut Vec<Arc<ChatMessage>> {
        let mut cache = self.row_cache.borrow_mut();
        cache.invalidate_workers();
        cache.invalidate_from(0);
        cache.activity = None;
        drop(cache);
        &mut self.messages
    }

    fn visible_running(&self) -> bool {
        self.running
    }

    /// Refresh snapshots after a mutation without a known message index.
    fn mark_active_dirty(&self) {
        self.active_dirty.set(true);
        self.activity_dirty.set(true);
        let mut cache = self.row_cache.borrow_mut();
        cache.invalidate_workers();
        cache.invalidate_from(0);
        cache.activity = None;
    }

    /// A row's height changed because the reader toggled a message.
    fn invalidate_message_row(&self, index: usize) {
        self.dirty_messages.borrow_mut().insert(index);
        self.row_cache.borrow_mut().invalidate_from(index);
    }

    fn invalidate_event_rows(&self, event: &agent_bridge::AgentEvent) {
        use agent_bridge::AgentEvent;
        if is_task_list_event(event) || matches!(event, AgentEvent::Usage(_) | AgentEvent::Title(_))
        {
            return;
        }
        let tail = self.messages.len().saturating_sub(1);
        let mut cache = self.row_cache.borrow_mut();
        let same_stream = matches!(
            (event, self.messages.last().map(|message| message.role)),
            (AgentEvent::Text(_), Some(ChatRole::Assistant))
                | (AgentEvent::Thinking(_), Some(ChatRole::Thinking))
        );
        if !same_stream {
            cache.activity = None;
        }
        let mut from = tail;
        match event {
            AgentEvent::Done | AgentEvent::Closed => {
                cache.invalidate_workers();
                from = self
                    .messages
                    .iter()
                    .rposition(|message| message.role == ChatRole::User)
                    .map_or(0, |index| index + 1);
            }
            AgentEvent::Subagents { tool_id, .. } => {
                cache.invalidate_workers();
                if let Some(index) = self.messages.iter().rposition(|message| {
                    message
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool.id.as_ref() == Some(tool_id))
                }) {
                    from = from.min(index);
                }
            }
            AgentEvent::ToolStart { id: Some(id), .. } => {
                if let Some(index) = self.messages.iter().rposition(|message| {
                    message.tool.as_ref().is_some_and(|tool| {
                        tool.id.as_ref() == Some(id) && !tool.workers.is_empty()
                    })
                }) {
                    from = from.min(index);
                }
            }
            AgentEvent::ToolUpdate { id, name, .. } | AgentEvent::ToolEnd { id, name, .. } => {
                if let Some(offset) = self
                    .messages
                    .iter()
                    .rev()
                    .take_while(|message| message.role != ChatRole::User)
                    .enumerate()
                    .filter_map(|(offset, message)| {
                        message
                            .tool
                            .as_ref()
                            .filter(|tool| {
                                tool.state == ToolState::Running
                                    && match id {
                                        Some(id) => tool.id.as_ref() == Some(id),
                                        None => tool.id.is_none() && tool.name == *name,
                                    }
                            })
                            .map(|_| offset)
                    })
                    .last()
                {
                    from = from.min(self.messages.len() - 1 - offset);
                }
                if matches!(event, AgentEvent::ToolEnd { .. })
                    && (is_agent_tool(name) || name.eq_ignore_ascii_case("wait"))
                {
                    cache.invalidate_workers();
                }
            }
            _ => {}
        }
        cache.invalidate_from(from);
        self.active_dirty.set(true);
        if !same_stream {
            self.activity_dirty.set(true);
        }
    }

    fn list_source(&self, running: bool) -> ChatListSource<'_> {
        ChatListSource {
            conversation_key: self.conversation_key.as_str(),
            messages: self.visible_messages(),
            running,
            list_state: &self.list_state,
            list_rows: &self.list_rows,
            list_key: &self.list_key,
            row_cache: &self.row_cache,
            active_dirty: &self.active_dirty,
            activity_dirty: &self.activity_dirty,
            dirty_messages: &self.dirty_messages,
            expanded_tool_groups: &self.expanded_tool_groups,
            expanded_turns: &self.expanded_turns,
        }
    }

    fn visible_conversation(&mut self) -> Option<&mut conversations::Log> {
        self.conversation.as_mut()
    }
}

/// Height guess for transcript rows that have never been laid out. The list
/// treats unknown rows as zero height, so a long unmeasured stretch collapses
/// the pixel space and one wheel tick can snap the transcript to its top or
/// bottom. Real measurements replace the hint as rows materialize.
const CHAT_ROW_HEIGHT_HINT: Pixels = px(96.0);

fn new_chat_list() -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(2048.0));
    list.set_follow_mode(FollowMode::Tail);
    list
}

/// Keep the list's item count in step with the transcript. Appending and
/// truncating preserve the reader's scroll anchor; a conversation switch
/// resets measurements.
fn sync_list_rows(
    list_state: &ListState,
    list_rows: &std::cell::Cell<usize>,
    list_key: &std::cell::Cell<u64>,
    count: usize,
    key: u64,
) {
    if key != list_key.get() {
        list_state.reset_with_uniform_height(count, CHAT_ROW_HEIGHT_HINT);
        list_state.set_follow_mode(FollowMode::Tail);
        list_key.set(key);
        list_rows.set(count);
        return;
    }
    let old = list_rows.get();
    if count > old {
        list_state.splice(old..old, count - old);
        // Appending a boundary finalizes the previous tail row (for
        // example a streaming answer becomes settled Markdown), so its
        // cached height must be remeasured too.
        if old > 0 {
            list_state.remeasure_items(old - 1..old);
        }
        // GPUI's seeding API preserves known heights as hints and gives new
        // rows a nonzero height so wheel and scrollbar jumps can reach them.
        list_state.clone().with_uniform_item_height(CHAT_ROW_HEIGHT_HINT);
    } else if count < old {
        list_state.splice(count..old, 0);
        if count > 0 {
            list_state.remeasure_items(count - 1..count);
        }
    }
    list_rows.set(count);
}

fn update_worked_turn_rows(
    list: &ListState,
    rows: &std::cell::Cell<usize>,
    header: Option<usize>,
    count: usize,
) {
    let old = rows.get();
    let Some(header) = header.filter(|&header| header < old.min(count)) else {
        return;
    };
    let details = header + 1;
    if count > old {
        list.pause_following_tail();
        list.splice(details..details, count - old);
        list.clone().with_uniform_item_height(CHAT_ROW_HEIGHT_HINT);
    } else if count < old {
        list.splice(details..details + (old - count), 0);
    }
    list.remeasure_items(header..(details + 1).min(count));
    rows.set(count);
}

impl WorkerView {
    fn new(index: usize, id: String, load: gpui::Task<()>) -> Self {
        let conversation_key = format!("worker-{index}-{id}");
        Self {
            index,
            id,
            conversation_key,
            messages: Vec::new(),
            expanded: HashSet::new(),
            full_output: HashSet::new(),
            expanded_tool_groups: HashMap::new(),
            expanded_turns: HashMap::new(),
            list_state: new_chat_list(),
            list_rows: std::cell::Cell::new(0),
            list_key: std::cell::Cell::new(0),
            row_cache: Default::default(),
            active_dirty: std::cell::Cell::new(false),
            activity_dirty: std::cell::Cell::new(false),
            dirty_messages: std::cell::RefCell::new(HashSet::new()),
            _load: Some(load),
        }
    }

    fn invalidate_message_row(&self, index: usize) {
        self.dirty_messages.borrow_mut().insert(index);
        self.row_cache.borrow_mut().invalidate_from(index);
    }

    fn list_source(&self, running: bool) -> ChatListSource<'_> {
        ChatListSource {
            conversation_key: self.conversation_key.as_str(),
            messages: &self.messages,
            running,
            list_state: &self.list_state,
            list_rows: &self.list_rows,
            list_key: &self.list_key,
            row_cache: &self.row_cache,
            active_dirty: &self.active_dirty,
            activity_dirty: &self.activity_dirty,
            dirty_messages: &self.dirty_messages,
            expanded_tool_groups: &self.expanded_tool_groups,
            expanded_turns: &self.expanded_turns,
        }
    }
}

impl ChatListSource<'_> {
    fn remeasure_row(&self, row: usize) {
        self.list_state.remeasure_items(row..row + 1);
    }

    fn sync_rows(&self, count: usize, key: u64) {
        sync_list_rows(self.list_state, self.list_rows, self.list_key, count, key);
    }
}

type PaneInputContext = (String, usize, PathBuf, Vec<String>, Entity<InputState>);

struct DiffPreview {
    directory: PathBuf,
    result: Option<Result<project_diff::ProjectDiff, String>>,
    expanded: HashSet<String>,
    tracked_collapsed: bool,
    untracked_collapsed: bool,
    request: u64,
    file_request: u64,
    file_view: Option<ReadOnlyFile>,
    file_only: bool,
}

struct InlinePatchState {
    // Keep the inputs alive while their addresses identify this cached rendering.
    _patch: SharedString,
    _theme: Arc<HighlightTheme>,
    diff: Option<project_diff::InlineDiff>,
}

struct ReadOnlyFile {
    path: String,
    state: ReadOnlyFileState,
}

enum ReadOnlyFileState {
    Loading,
    Error(String),
    Ready {
        input: Entity<InputState>,
        deleted: bool,
    },
}

fn diff_counts(
    counts: (Option<usize>, Option<usize>),
    added: gpui::Rgba,
    removed: gpui::Rgba,
    muted: Hsla,
) -> Div {
    let row = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .flex_shrink_0()
        .text_size(px(11.0));
    match counts {
        (Some(additions), Some(deletions)) => row
            .child(div().text_color(added).child(format!("+{additions}")))
            .child(div().text_color(removed).child(format!("-{deletions}"))),
        _ => row.child(div().text_color(muted).child("binary")),
    }
}

/// One terminal inside a session's screen. A session (a sidebar tab) owns a
/// split tree of panes; each pane is a full, independent terminal whose PTY
/// keeps running while another session's screen is shown.
pub struct TerminalPane {
    pub id: usize,
    /// Stable identity across restarts, used for scrollback and conversation files.
    pub uid: String,
    /// Last working directory reported by shell integration (OSC 7 / 7502).
    pub cwd: Option<PathBuf>,
    pub generation: String,
    pub native_session_id: Option<String>,
    pub native_session_reporter: Option<String>,
    pub native_turn_id: Option<String>,
    pub native_sequence: HashMap<String, u64>,
    pub native_reporter_generation: HashMap<String, String>,
    pub pending_approvals: std::collections::HashSet<String>,
    pub native_status: Option<SessionStatus>,
    pub native_acknowledged: bool,
    pub osc_report_active: bool,
    pub terminal: Entity<TerminalView>,
    markdown_preview: Option<(PathBuf, SharedString)>,
    diff_preview: Option<DiffPreview>,
    files_surface: Option<(Entity<files_surface::FilesSurface>, gpui::Subscription)>,
    files_active: bool,
    files_restore: Option<files_surface::SavedFiles>,
    /// Agent pane that owns this reusable preview. Preview panes reuse themselves.
    preview_owner: Option<usize>,
    one_mode: bool,
    legacy_chat: bool,
    regular_focus: Option<usize>,
    tool_panel: bool,
    tool_panel_tab: Option<usize>,
    tool_panel_hidden: bool,
    tool_panel_chooser: Option<gpui::FocusHandle>,
    tool_panel_file_restore: Option<PathBuf>,
    tool_panel_scroll: Option<gpui::ScrollHandle>,
    tool_panel_chat_ratio: Option<f32>,
    /// Invalidates background work whenever the preview content is replaced.
    preview_generation: u64,
    pub status: SessionStatus,
    pub detection: status::DetectionResult,
    pub title: String,
    /// Title as shown in the pane header (truncated to 160 chars), cached so
    /// the render pass does not re-scan and re-allocate it every frame.
    pub display_title: SharedString,
    pub pid: Option<u32>,
    /// Reused scrape buffer: the poller reads the screen into this instead of
    /// allocating a fresh `Vec<String>` every 500 ms per pane.
    pub screen_scratch: Vec<String>,
    /// (screen revision, focused, exited, foreground snapshot) from the last
    /// poll tick. The revision lets an idle pane skip rebuilding its screen
    /// strings entirely; the process snapshot is compared by `Arc` identity,
    /// which is O(1) because the `/proc` cache hands back the same allocation
    /// while the foreground process group is unchanged.
    pub poll_cache: Option<(u64, bool, bool, Arc<[String]>)>,
    /// Screen fingerprint at the moment the user last looked at a finished or
    /// failed terminal. While the screen still matches, the sticky Done/Error
    /// badge stays acknowledged (Idle); any new output invalidates it and lets
    /// a fresh Done/Error be raised.
    pub ack_screen: Option<u64>,
    /// A recognized agent CLI was the foreground process on the previous tick.
    /// Its disappearance means the agent finished (Done), like shob.
    pub agent_active: bool,
    pub detector: status::Detector,
    /// Shob-style seen/recency bookkeeping for this pane's finalized status.
    pub attention: attention::Attention,
    /// The agent's own name from its OSC 7501 report (`app`), when it reports one.
    pub app: Option<SharedString>,
    /// Sticky: this pane hosted a recognized agent. Cleared only when recreated.
    pub is_agent: bool,
    /// Workers reported by this pane's agent, keyed by their `agent_id`.
    pub sub_agents: HashMap<String, attention::SubAgent>,
    chat: PaneChat,
    pub input_state: Option<Entity<InputState>>,
    /// Text typed before the last quit, applied when the input is first built.
    pub restore_draft: Option<String>,
    /// Drops the `InputEvent::Change` subscription that keeps ghost text and
    /// the completion menu in sync with what is typed.
    pub input_subscription: Option<gpui::Subscription>,
    /// Shell history loaded from disk, shared by every pane.
    pub shell_history: Arc<Vec<String>>,
    /// Commands executed from this pane's input bar, newest last.
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
    pub pending_draft: String,
    /// Prevent duplicate submissions while physical PTY writes are outstanding.
    input_submitting: bool,
    input_cancel: Option<smol::channel::Sender<()>>,
    /// Open completion menu (Shob-style suggestions above the input).
    pub completion_menu: Option<CompletionMenuState>,
    pub bottom_input_visible: bool,
    pub plus_menu_open: bool,
    model_picker_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    permission_menu_open: bool,
    permission_toggle_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    workspace_menu_open: bool,
    workspace_toggle_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    branch_picker: Option<BranchPickerState>,
    branch_toggle_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    project_menu_open: bool,
    project_toggle_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// Files staged in the input bar; dropped, pasted, or picked. They are
    /// sent to the agent as file paths when the prompt is submitted.
    pub attachments: Vec<attachments::AttachedFile>,
    /// True while the shell reports a command owns the terminal.
    pub command_running: bool,
    /// Keeps the terminal's `ShellEvent` subscription alive for this pane.
    pub terminal_subscription: Option<gpui::Subscription>,
    idle_resources_released: bool,
    /// Active -> Idle -> Hibernated. Hibernation drops the terminal grid, so
    /// `idle_resources_released` is only the lighter, cache-sized step.
    lifecycle: session_lifecycle::PaneLifecycle,
}

/// Commands handled by the agent composer itself.
const AGENT_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show available commands"),
    ("/diff", "Show project changes"),
    ("/new", "Start a new chat"),
    ("/copy", "Copy the last assistant reply"),
    ("/version", "Show the application version"),
    ("/auth", "Manage provider authentication"),
    ("/model", "Choose a model"),
    ("/fast", "Toggle fast reasoning"),
    ("/thinking", "Set reasoning effort"),
];

fn agent_slash_completions(value: &str) -> Vec<completion::CompletionItem> {
    if value.starts_with('@') && !value.chars().any(char::is_whitespace) {
        return if "@browser".starts_with(&value.to_lowercase()) {
            vec![completion::CompletionItem {
                value: "@browser".into(),
                display: "@browser".into(),
                detail: "Use agent-browser for this request".into(),
                kind: completion::CompletionKind::Command,
                is_dir: false,
            }]
        } else {
            Vec::new()
        };
    }
    if !value.starts_with('/') || value.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let query = value.to_lowercase();
    AGENT_SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(&query))
        .map(|(name, description)| completion::CompletionItem {
            value: (*name).into(),
            display: (*name).into(),
            detail: (*description).into(),
            kind: completion::CompletionKind::Command,
            is_dir: false,
        })
        .collect()
}

/// One visible completion popup for a pane.
pub struct CompletionMenuState {
    /// Byte range in the input value that accepting replaces.
    pub start: usize,
    pub end: usize,
    /// The token these candidates were computed for, so typing resets the
    /// highlight instead of carrying it into a different list.
    pub token: String,
    pub items: Vec<completion::CompletionItem>,
    pub selected: usize,
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        #[cfg(unix)]
        shob_compat::unregister(&self.generation);
    }
}

impl TerminalPane {
    pub fn new(
        id: usize,
        generation: String,
        terminal: Entity<TerminalView>,
        title: SharedString,
        display_title: SharedString,
        pid: Option<u32>,
    ) -> Self {
        Self {
            id,
            uid: paths::uid("p"),
            cwd: None,
            generation,
            native_session_id: None,
            native_session_reporter: None,
            native_turn_id: None,
            native_sequence: HashMap::new(),
            native_reporter_generation: HashMap::new(),
            pending_approvals: std::collections::HashSet::new(),
            native_status: None,
            native_acknowledged: false,
            osc_report_active: false,
            terminal,
            markdown_preview: None,
            diff_preview: None,
            files_surface: None,
            files_active: false,
            files_restore: None,
            preview_owner: None,
            legacy_chat: false,
            one_mode: false,
            regular_focus: None,
            tool_panel: false,
            tool_panel_tab: None,
            tool_panel_hidden: false,
            tool_panel_chooser: None,
            tool_panel_file_restore: None,
            tool_panel_scroll: None,
            tool_panel_chat_ratio: None,
            preview_generation: 0,
            status: SessionStatus::Idle,
            detection: status::DetectionResult::new(
                SessionStatus::Idle,
                status::EvidenceSource::NoSignal,
                status::Confidence::None,
            ),
            title: title.to_string(),
            display_title,
            pid,
            screen_scratch: Vec::new(),
            poll_cache: None,
            ack_screen: None,
            agent_active: false,
            detector: status::Detector::default(),
            attention: Default::default(),
            app: None,
            is_agent: false,
            sub_agents: HashMap::new(),
            chat: PaneChat::default(),
            input_state: None,
            restore_draft: None,
            input_subscription: None,
            shell_history: shell_history::cached(),
            command_history: Vec::new(),
            history_index: None,
            pending_draft: String::new(),
            input_submitting: false,
            input_cancel: None,
            completion_menu: None,
            bottom_input_visible: true,
            plus_menu_open: false,
            model_picker_bounds: Rc::new(std::cell::Cell::new(Bounds::default())),
            permission_menu_open: false,
            permission_toggle_bounds: Rc::new(std::cell::Cell::new(Bounds::default())),
            workspace_menu_open: false,
            workspace_toggle_bounds: Rc::new(std::cell::Cell::new(Bounds::default())),
            branch_picker: None,
            branch_toggle_bounds: Rc::new(std::cell::Cell::new(Bounds::default())),
            project_menu_open: false,
            project_toggle_bounds: Rc::new(std::cell::Cell::new(Bounds::default())),
            attachments: Vec::new(),
            command_running: false,
            terminal_subscription: None,
            idle_resources_released: false,
            lifecycle: session_lifecycle::PaneLifecycle::new(std::time::Instant::now()),
        }
    }

    fn has_live_work(&self) -> bool {
        self.command_running
            || self.input_submitting
            || self.chat.running
            || !self.chat.requests.is_empty()
            || !self.chat.follow_ups.is_empty()
            || !self.pending_approvals.is_empty()
            || self.chat.auth_refresh_pending
            || matches!(self.status, SessionStatus::Working | SessionStatus::Blocked)
            || self.sub_agents.values().any(|agent| {
                matches!(
                    agent.status,
                    SessionStatus::Working | SessionStatus::Blocked
                )
            })
    }

    /// Show the editor at the shell prompt; running commands and alternate
    /// screen programs receive input directly through the terminal.
    pub fn should_show_bottom_input(&self, cx: &App) -> bool {
        if self.chat.active {
            return true;
        }
        if self.tool_panel || !self.bottom_input_visible {
            return false;
        }

        // While a command owns stdin, the editor is hidden and raw input goes
        // straight to the PTY (sudo, ssh, read, interactive CLIs).
        if self.command_running {
            return false;
        }

        // Alternate screen mode (vim, nano, htop, less, etc.) always hides the bottom bar
        if self.terminal.read(cx).is_alt_screen() {
            return false;
        }

        true
    }
}

/// One sidebar tab with its own split-layout tree. Background work stays live;
/// idle, unselected panes release disposable runtime and rendering resources.
pub struct Session {
    pub id: usize,
    /// Stable identity across restarts.
    pub uid: String,
    /// OSC title of the focus pane (synced in the poller / title callback).
    pub title: SharedString,
    /// Title as shown in the sidebar (truncated), cached.
    pub display_title: SharedString,
    /// Path of the AI agent / command logo shown in the sidebar.
    pub logo: SharedString,
    pub pane_tree: Option<PaneNode>,
    pub maximized_pane_id: Option<usize>,
    pub panes: Vec<TerminalPane>,
    /// Highest-ranked pane status, for the sidebar badge and title bar.
    pub status: SessionStatus,
}

/// Status poll period. Process status cannot be evented portably across the
/// platforms this app supports, so this bounded timer is the change detector;
/// the work it triggers is small (`/proc` reads now happen off the UI thread).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
#[cfg(unix)]
static FANOUT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn latch_completion(previous: SessionStatus, next: SessionStatus) -> SessionStatus {
    // Completion persistence is owned by the lifecycle reducer: idle/focus
    // observations cannot clear it, while fresh work always can.
    if matches!(previous, SessionStatus::Done | SessionStatus::Error)
        && matches!(
            next,
            SessionStatus::Idle | SessionStatus::Active | SessionStatus::Unknown
        )
    {
        previous
    } else {
        next
    }
}

#[cfg(test)]
#[test]
fn completed_status_survives_idle_and_focus_but_new_work_replaces_it() {
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Idle),
        SessionStatus::Done
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Active),
        SessionStatus::Done
    );
    assert_eq!(
        latch_completion(SessionStatus::Error, SessionStatus::Unknown),
        SessionStatus::Error
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Working),
        SessionStatus::Working
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Blocked),
        SessionStatus::Blocked
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneNode {
    Leaf(usize),
    Split {
        id: usize,
        axis: gpui::Axis,
        left: Box<PaneNode>,
        right: Box<PaneNode>,
    },
}
