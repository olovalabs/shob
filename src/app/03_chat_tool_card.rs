fn chat_tool_card(
    pane_id: usize,
    index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    host: &UiHost,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_show_more: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    if chat_batch::is_batch(&tool.name) && chat_batch::known_count(tool).is_some() {
        let mut calls = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(chat_tool_header(pane_id, index, tool, tokens, on_toggle));
        if tool.expanded {
            calls = calls.child(chat_batch::details(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .pl(px(12.0)),
                pane_id,
                index,
                tool,
                tokens,
                host,
            ));
        }
        if let Some(marker) = tool_paint_marker(tool.id.as_deref(), false) {
            calls = calls.relative().child(marker);
        }
        if !tool.output.is_empty()
            && let Some(marker) = tool_paint_marker(tool.id.as_deref(), true)
        {
            calls = calls.relative().child(marker);
        }
        return calls;
    }
    let mut card = div()
        .when(matches!(tool.name.as_str(), "edit" | "write" | "apply_patch" | "str_replace_editor"), |card| {
            card.bg(tokens.card_bg).border_1().border_color(tokens.card_border).rounded(px(8.)).p(px(8.))
        })
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .child(chat_tool_header(pane_id, index, tool, tokens, on_toggle));

    if let Some(marker) = tool_paint_marker(tool.id.as_deref(), false) {
        card = card.relative().child(marker);
    }

    if tool.expanded {
        card = card.child(chat_tool_content(
            pane_id,
            index,
            tool,
            tokens,
            on_show_more,
        ));
    }
    card
}

fn chat_thinking_row(
    pane_id: usize,
    index: usize,
    message: &ChatMessage,
    running: bool,
    render: ChatRenderContext<'_>,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let ChatRenderContext { tokens, window, cx, measurement_ready } = render;
    let expanded = message.expanded;
    let running = running && message.streaming;
    let heading_end = message
        .text
        .char_indices()
        .nth(81)
        .map_or(message.text.len(), |(index, _)| index);
    let heading = clip_tool_text(first_line(&message.text[..heading_end]), 80).0;
    let label = if running { "Thinking" } else { "Thought" };

    let title = div()
        .flex_shrink_0()
        .text_size(px(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(tokens.text_primary)
        .child(label);
    // The label already reads "Thinking" while streaming and "Thought" after.
    // A repeating pulse here would repaint the window for the whole reasoning
    // phase (AGENTS.md: no continuously repainting animations).
    let title = title.into_any_element();

    let mut header = div()
        .id(SharedString::from(format!(
            "thinking-row-{pane_id}-{index}"
        )))
        .w_full()
        .min_w_0()
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(MouseButton::Left, on_toggle)
        .child(title);

    if !expanded && !heading.is_empty() {
        header = header.child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(13.0))
                .text_color(tokens.text_muted)
                .child(heading),
        );
    }

    header = header.child(div().flex_1()).child(
        svg()
            .path(if expanded {
                "icons/chevron-down.svg"
            } else {
                "icons/chevron-right.svg"
            })
            .size(px(12.0))
            .flex_shrink_0()
            .text_color(tokens.text_muted),
    );

    let mut row = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .debug_selector(move || format!("thinking-row-{index}"))
        .child(header);

    if expanded {
        let body_id = SharedString::from(format!("thinking-body-{pane_id}-{index}"));
        let mut body = div().w_full().min_w_0().flex().flex_col().gap(px(6.0));
        if let Some(view) = message.stream.as_ref() {
            if !view.settled.is_empty() {
                body = body.child(chat_markdown(
                    body_id,
                    view.settled.clone(),
                    px(12.5),
                    rems(0.4),
                    tokens.text_muted,
                    ChatRenderContext { tokens, window, cx, measurement_ready },
                ));
            }
            if !view.tail.is_empty() {
                body = body.child(chat_stream_tail(
                    SharedString::from(format!("thinking-body-{pane_id}-{index}-live")),
                    view,
                    px(12.5),
                    rems(0.4),
                    tokens.text_muted,
                    ChatRenderContext { tokens, window, cx, measurement_ready },
                ));
            }
        } else {
            body = body.child(chat_markdown(
                body_id,
                message.markdown(),
                px(12.5),
                rems(0.4),
                tokens.text_muted,
                ChatRenderContext { tokens, window, cx, measurement_ready },
            ));
        }
        row = row.child(
            div()
                .w_full()
                .min_w_0()
                .ml(px(12.0))
                .pl(px(12.0))
                .border_l_1()
                .border_color(tokens.border_subtle)
                .py(px(6.0))
                .debug_selector(move || format!("thinking-body-{index}"))
                .child(body),
        );
    }
    row
}

fn update_chat_tool(
    messages: &mut Vec<Arc<ChatMessage>>,
    id: Option<String>,
    name: String,
    output: String,
    state: ToolState,
) {
    // An absent ID cannot distinguish concurrent calls with the same name.
    // Associate those events with the oldest matching running call.
    let existing = messages
        .iter()
        .rev()
        .take_while(|message| message.role != ChatRole::User)
        .enumerate()
        .filter_map(|(offset, message)| {
            message
                .tool
                .as_ref()
                .filter(|tool| tool.state == ToolState::Running)
                .filter(|tool| match id.as_ref() {
                    Some(id) => tool.id.as_ref() == Some(id),
                    None => tool.id.is_none() && tool.name == name,
                })
                .map(|_| offset)
        })
        .last()
        .map(|offset| messages.len() - 1 - offset);
    let workers = if state == ToolState::Completed {
        subagent_chat::legacy(&name, &output)
    } else {
        Vec::new()
    };
    let batch_result = (state == ToolState::Completed && chat_batch::is_batch(&name))
        .then(|| chat_batch::BatchResult::parse(&output).map(Arc::new))
        .flatten();
    let failure_preview = (state == ToolState::Failed && chat_batch::is_batch(&name))
        .then(|| chat_batch::failure_preview(&output))
        .flatten();
    let (full, preview, truncated) = if let Some(preview) = failure_preview {
        (
            SharedString::from(output),
            SharedString::from(preview),
            true,
        )
    } else {
        tool_output_fields(output)
    };
    if let Some(index) = existing {
        let message = Arc::make_mut(&mut messages[index]);
        let tool = message.tool.as_mut().expect("matched a tool message");
        subagent_chat::update_workers(&mut tool.workers, workers);
        if state == ToolState::Completed {
            match tool.display.view.title.as_str() {
                "Webfetch" => tool.web_sources = web_sources(&name, &tool.display.view.subtitle),
                "Web Search" => tool.web_sources = web_sources(&name, &full),
                _ => {}
            }
        }
        tool.output = full;
        tool.preview = preview;
        tool.preview_truncated = truncated;
        tool.batch_result = batch_result;
        tool.state = state;
        if chat_batch::is_batch(&tool.name) && state != ToolState::Running {
            tool.expanded = false;
        }
    } else {
        let display = ToolDisplay::new(&name, "");
        messages.push(Arc::new(ChatMessage {
            role: ChatRole::Tool,
            text: String::new(),
            tool: Some(ChatTool {
                workers,
                web_sources: if state == ToolState::Completed {
                    web_sources(&name, &full)
                } else {
                    Vec::new()
                },
                id,
                name,
                output: full,
                preview,
                preview_truncated: truncated,
                batch_result,
                state,
                expanded: false,
                expanded_calls: [false; embedded::MAX_BATCH_CALLS],
                display,
                show_full: false,
            }),
            expanded: false,
            streaming: false,
            stream: None,
            markdown: std::sync::OnceLock::new(),
            literal: std::sync::OnceLock::new(),
            turn: None,
            source_timestamp: None,
            worked_ms: None,
            worked_end: 0,
            worked_answer: None,
        }));
    }
}

static PROVIDER_ICONS: std::sync::LazyLock<HashMap<String, SharedString>> =
    std::sync::LazyLock::new(collect_provider_icons);
static CUSTOM_PROVIDER_IDS: std::sync::LazyLock<parking_lot::RwLock<HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

fn collect_provider_icons() -> HashMap<String, SharedString> {
        assets::AppAssets::iter()
            .filter_map(|path| {
                let stem = path.strip_prefix("icons/provider/")?.strip_suffix(".svg")?;
                Some((stem.to_owned(), SharedString::from(path.into_owned())))
            })
            .collect()
}

fn load_provider_icons() {
    std::sync::LazyLock::force(&PROVIDER_ICONS);
}

fn provider_icon_stem(id: &str) -> &str {
    match id {
        "opencode" | "opencode-zen" => "opencode",
        "google-antigravity" | "google-gemini-cli" => "google",
        "xai-oauth" => "xai",
        "lm-studio" => "lmstudio",
        "ollama" => "ollama-cloud",
        "fireworks" => "fireworks-ai",
        "novita" => "novita-ai",
        "together" => "togetherai",
        "cloudflare" => "cloudflare-workers-ai",
        "azure-openai" => "azure",
        "bedrock" | "aws-bedrock" => "amazon-bedrock",
        "copilot" => "github-copilot",
        "kimi" | "kimi-code" => "kimi-for-coding",
        "zhipu" => "zhipuai",
        "moonshot" => "moonshotai",
        "openai-codex" | "openai-codex-device" => "openai",
        "gitlab-duo" | "gitlab-duo-agent" => "gitlab",
        other => other,
    }
}

/// Providers added through the OpenAI-compatible API form (or hand-added to
/// `models.json`) get the OpenAI-compatible logo; the rest get monochrome stems.
fn provider_icon_path(id: &str) -> SharedString {
    provider_icon_path_for(id, is_custom_provider(id))
}

fn provider_icon_path_for(id: &str, custom: bool) -> SharedString {
    if custom {
        return SharedString::from("logos/openai-compatible.svg");
    }
    std::sync::LazyLock::get(&PROVIDER_ICONS)
        .and_then(|icons| icons.get(provider_icon_stem(id)))
        .cloned()
        .unwrap_or_else(|| "icons/provider/synthetic.svg".into())
}

fn picker_provider_icon(id: &str, custom: bool) -> SharedString {
    if id == "anthropic" && !custom {
        return "icons/provider/claude-agent.svg".into();
    }
    provider_icon_path_for(id, custom)
}

fn picker_effort_label(level: &str) -> &str {
    match level {
        "off" | "none" => "None",
        "low" => "Low",
        "xhigh" => "Extra High",
        "max" => "Max",
        level => reasoning_effort_label(level),
    }
}

fn is_custom_provider(id: &str) -> bool {
    CUSTOM_PROVIDER_IDS.read().contains(id)
}

/// Human-readable provider title and optional descriptive subtitle.
fn provider_metadata(id: &str) -> (&'static str, &'static str) {
    match id {
        "opencode-go" => ("OpenCode Go", "Low cost subscription for everyone"),
        "opencode-zen" | "opencode" => ("OpenCode Console", "Reliable optimized models"),
        "openai" => ("OpenAI", ""),
        "anthropic" => ("Anthropic", ""),
        "google" => ("Google", ""),
        "github-copilot" | "copilot" => ("GitHub Copilot", ""),
        "google-antigravity" => ("Google Antigravity", ""),
        "google-gemini-cli" => ("Gemini CLI", ""),
        "commandcode" => ("Command Code", ""),
        "cursor" => ("Cursor", ""),
        "devin" | "devin-cli" | "devin-windsurf" => ("Devin", ""),
        "openrouter" => ("OpenRouter", ""),
        "deepseek" => ("DeepSeek", ""),
        "ollama" | "ollama-cloud" => ("Ollama", ""),
        "groq" => ("Groq", ""),
        "mistral" | "mistral-api" => ("Mistral", ""),
        "together" | "togetherai" => ("Together AI", ""),
        "xai" => ("xAI", ""),
        "xai-oauth" => ("xAI Grok", ""),
        "lmstudio" | "lm-studio" => ("LM Studio", ""),
        "bedrock" | "amazon-bedrock" | "aws-bedrock" => ("Amazon Bedrock", ""),
        "azure" | "azure-openai" => ("Azure OpenAI", ""),
        "cloudflare" | "cloudflare-workers-ai" => ("Cloudflare", ""),
        "cohere" => ("Cohere", ""),
        "cerebras" => ("Cerebras", ""),
        "fireworks" | "fireworks-ai" => ("Fireworks AI", ""),
        "perplexity" => ("Perplexity", ""),
        "minimax" | "minimax-api" | "minimax-code" | "minimax-code-cn" => ("MiniMax", ""),
        "moonshot" | "moonshotai" => ("Moonshot AI", ""),
        "alibaba" | "alibaba-coding-plan" => ("Alibaba", ""),
        "huggingface" => ("Hugging Face", ""),
        "zai" | "zai-coding-plan" => ("Z.AI", ""),
        "kimi" | "kimi-code" => ("Kimi Code", ""),
        _ => ("", ""),
    }
}

fn provider_display_name(id: &str) -> String {
    let (name, _) = provider_metadata(id);
    if !name.is_empty() {
        name.to_string()
    } else {
        id.to_string()
    }
}

fn provider_featured_rank(id: &str) -> usize {
    match id {
        "openai" => 0,
        "anthropic" => 1,
        "google" => 2,
        "github-copilot" | "copilot" => 3,
        "google-antigravity" => 4,
        _ => usize::MAX,
    }
}

/// Fallback sidebar title when the first prompt's title request fails.
fn agent_title_from_prompt(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('/'))?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(match collapsed.char_indices().nth(64) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    })
}

/// The `provider/model` selector both agent backends are launched with.
///
/// Model ids may themselves contain slashes (`commandcode` +
/// `deepseek/deepseek-v4-flash`), so the provider is always prefixed instead of
/// letting the backend guess a provider from the id. Every launch path must use
/// this, otherwise a pane runs a different model than the chip shows.
fn agent_model_selector(model: &agent_auth::AgentModel) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// Assistant text stops streaming whenever the agent moves on to thinking,
/// activity, a tool call, an error, or the end of the turn. The next text
/// event starts a fresh streaming message, so completed text gets the full
/// markdown render again. Completed thinking folds into its compact header.
fn finish_chat_streaming(messages: &mut [Arc<ChatMessage>]) {
    // Only the newest message can be streaming: every other role calls this
    // before pushing, so scanning the whole transcript here is wasted work.
    if let Some(message) = messages.last_mut()
        && message.streaming
    {
        let message = Arc::make_mut(message);
        message.streaming = false;
        if let Some(view) = message.stream.take() {
            view.settled
                .finish(SharedString::from(message.text.clone()));
            let _ = message.markdown.set(view.settled);
        }
        if message.role == ChatRole::Thinking {
            message.expanded = false;
        }
    }
}

/// Cap live scans, copies and parser input independently of unfinished block size.
/// Once oversized, keep a plain suffix until finalization; never fragment a
/// fence/list into separately parsed pieces with lost syntax or numbering.
fn refresh_stream_view(message: &mut ChatMessage, scan: bool) {
    if message.stream.is_none() {
        let _ = message.markdown.take();
    }
    let previous = message.stream.as_ref().map_or(0, |view| view.settled.len());
    let plain_tail = message.stream.as_ref().is_some_and(|view| view.plain_tail)
        || message.text.len() - previous > STREAM_LIVE_BYTES;
    let split_at = if scan && !plain_tail {
        let (_, tail) = split_streaming_markdown(&message.text[previous..]);
        message.text.len() - tail.len()
    } else {
        previous
    };
    let mut tail_at = split_at.max(message.text.len().saturating_sub(STREAM_LIVE_BYTES));
    while !message.text.is_char_boundary(tail_at) {
        tail_at += 1;
    }
    let tail = SharedString::from(&message.text[tail_at..]);
    if let Some(view) = message.stream.as_mut() {
        if previous != split_at {
            view.settled
                .push(SharedString::from(&message.text[previous..split_at]));
        }
        view.tail = tail;
        view.plain_tail = plain_tail;
    } else {
        let settled = MarkdownBlocks::default();
        if split_at > 0 {
            settled.push(SharedString::from(&message.text[..split_at]));
        }
        message.stream = Some(StreamView {
            settled,
            tail,
            plain_tail,
        });
    }
}

fn chat_timestamp_ms() -> Option<u64> {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn finish_worked_turn(messages: &mut [Arc<ChatMessage>], completed_at: Option<u64>) -> Option<usize> {
    let user = messages.iter().rposition(|message| message.role == ChatRole::User)?;
    let first = user + 1;
    if first == messages.len() || messages[first].worked_end != 0 { return None; }
    let elapsed = completed_at.zip(messages[user].source_timestamp).and_then(|(end, start)| end.checked_sub(start));
    let answer = messages[first..].iter().rposition(|message| matches!(message.role, ChatRole::Assistant | ChatRole::Error) && !message.text.is_empty()).map(|index| first + index);
    let end = messages.len();
    let message = Arc::make_mut(&mut messages[first]);
    message.worked_ms = elapsed;
    message.worked_end = end;
    message.worked_answer = answer;
    Some(first)
}

fn finish_chat_tools(messages: &mut [Arc<ChatMessage>]) {
    for message in messages
        .iter_mut()
        .rev()
        .take_while(|message| message.role != ChatRole::User)
    {
        if message
            .tool
            .as_ref()
            .is_some_and(|tool| tool.state == ToolState::Running)
            && let Some(tool) = Arc::make_mut(message).tool.as_mut()
        {
            tool.state = ToolState::Unknown;
            if tool.name == embedded::subagent_tool::NAME {
                tool.workers.iter_mut().for_each(subagent_chat::Worker::mark_interrupted);
            }
            if chat_batch::is_batch(&tool.name) {
                tool.expanded = false;
            }
        }
    }
}

const TASK_LIST_COMPLETION_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

fn is_task_list_event(event: &agent_bridge::AgentEvent) -> bool {
    use agent_bridge::AgentEvent;
    matches!(
        event,
        AgentEvent::ToolStart { name, .. }
            | AgentEvent::ToolUpdate { name, .. }
            | AgentEvent::ToolEnd { name, .. }
            if name == embedded::task_list::NAME
    )
}

fn task_list_update(event: &agent_bridge::AgentEvent) -> Option<embedded::task_list::TaskList> {
    match event {
        agent_bridge::AgentEvent::ToolEnd {
            name,
            output,
            failed: false,
            ..
        } if name == embedded::task_list::NAME => serde_json::from_str(output).ok(),
        _ => None,
    }
}

fn active_task_index(list: &embedded::task_list::TaskList) -> Option<usize> {
    list.tasks
        .iter()
        .position(|task| task.status == embedded::task_list::TaskStatus::InProgress)
}

fn task_list_completed(list: &embedded::task_list::TaskList) -> bool {
    !list.tasks.is_empty()
        && list
            .tasks
            .iter()
            .all(|task| task.status == embedded::task_list::TaskStatus::Completed)
}

fn push_chat_event(messages: &mut Vec<Arc<ChatMessage>>, event: agent_bridge::AgentEvent) {
    let at_ms = chat_timestamp_ms();
    push_chat_event_at(messages, event, at_ms);
}

fn push_chat_event_at(messages: &mut Vec<Arc<ChatMessage>>, event: agent_bridge::AgentEvent, at_ms: Option<u64>) {
    let previous = messages.len();
    push_chat_event_inner(messages, event);
    for message in &mut messages[previous..] {
        Arc::make_mut(message).source_timestamp = at_ms;
    }
}

fn push_chat_event_inner(messages: &mut Vec<Arc<ChatMessage>>, event: agent_bridge::AgentEvent) {
    if is_task_list_event(&event) {
        return;
    }
    let (role, text) = match event {
        agent_bridge::AgentEvent::ToolStart { id, name, input } => {
            if let Some(id) = id.as_deref() {
                crate::stream_timing::tool_applied(id);
            }
            // Finalize the streaming block before the tool card becomes the
            // newest message; finalization only looks at the tail.
            finish_chat_streaming(messages);
            if let Some(id) = id.as_deref()
                && let Some(tool) = messages.iter_mut().rev().find_map(|message| {
                    let tool = message.tool.as_ref()?;
                    if tool.id.as_deref() != Some(id) || tool.workers.is_empty() {
                        return None;
                    }
                    Arc::make_mut(message).tool.as_mut()
                })
            {
                tool.display = ToolDisplay::new(&name, &input);
                tool.name = name;
                return;
            }
            let display = ToolDisplay::new(&name, &input);
            let expanded = chat_batch::is_batch(&name);
            messages.push(Arc::new(ChatMessage {
                role: ChatRole::Tool,
                text: String::new(),
                tool: Some(ChatTool {
                    workers: Vec::new(),
                    web_sources: Vec::new(),
                    id,
                    name,
                    output: SharedString::from(""),
                    preview: SharedString::from(""),
                    preview_truncated: false,
                    batch_result: None,
                    state: ToolState::Running,
                    expanded,
                    expanded_calls: [false; embedded::MAX_BATCH_CALLS],
                    display,
                    show_full: false,
                }),
                expanded: false,
                streaming: false,
                stream: None,
                markdown: std::sync::OnceLock::new(),
                literal: std::sync::OnceLock::new(),
                turn: None,
                source_timestamp: None,
                worked_ms: None,
            worked_end: 0,
            worked_answer: None,
            }));
            return;
        }
        agent_bridge::AgentEvent::ToolUpdate { id, name, output } => {
            finish_chat_streaming(messages);
            update_chat_tool(messages, id, name, output, ToolState::Running);
            return;
        }
        agent_bridge::AgentEvent::ToolEnd {
            id,
            name,
            output,
            failed,
        } => {
            finish_chat_streaming(messages);
            update_chat_tool(
                messages,
                id,
                name,
                output,
                if failed {
                    ToolState::Failed
                } else {
                    ToolState::Completed
                },
            );
            return;
        }
        agent_bridge::AgentEvent::Subagents { tool_id, workers } => {
            if let Some(message) = messages.iter_mut().rev().find(|message| {
                message
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.id.as_deref() == Some(tool_id.as_str()))
            }) {
                if let Some(tool) = Arc::make_mut(message).tool.as_mut() {
                    subagent_chat::update_workers(&mut tool.workers, workers);
                }
            } else {
                push_chat_event(
                    messages,
                    agent_bridge::AgentEvent::ToolStart {
                        id: Some(tool_id),
                        name: "task".into(),
                        input: String::new(),
                    },
                );
                if let Some(tool) = messages
                    .last_mut()
                    .and_then(|message| Arc::make_mut(message).tool.as_mut())
                {
                    subagent_chat::update_workers(&mut tool.workers, workers);
                }
            }
            return;
        }
        agent_bridge::AgentEvent::Text(text) => (ChatRole::Assistant, text),
        agent_bridge::AgentEvent::Thinking(text) => (ChatRole::Thinking, text),
        agent_bridge::AgentEvent::Status(text) => (ChatRole::Status, text),
        agent_bridge::AgentEvent::Tool(text) => (ChatRole::Tool, text),
        agent_bridge::AgentEvent::Error(text) => (ChatRole::Error, text),
        // The session title is metadata, not transcript content.
        agent_bridge::AgentEvent::Title(_)
        | agent_bridge::AgentEvent::Usage(_)
        | agent_bridge::AgentEvent::Done
        | agent_bridge::AgentEvent::Closed => return,
    };
    if messages.last().is_some_and(|last| last.role != role) {
        finish_chat_streaming(messages);
    }
    if matches!(role, ChatRole::Assistant | ChatRole::Thinking)
        && let Some(last) = messages.last_mut().filter(|message| message.role == role)
    {
        let last = Arc::make_mut(last);
        last.text.push_str(&text);
        // Reasoning streams like assistant text: settled blocks stay cached and
        // only the live tail is re-parsed, instead of reparsing the whole block
        // on every throttled tick.
        last.streaming = true;
        refresh_stream_view(last, text.contains('\n'));
        return;
    }
    let mut message = ChatMessage::new(role, text);
    if matches!(role, ChatRole::Assistant | ChatRole::Thinking) {
        message.streaming = true;
        refresh_stream_view(&mut message, true);
    }
    messages.push(Arc::new(message));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentDialogPage {
    Auth,
    Models,
}

#[derive(Clone, PartialEq, Eq)]
enum ModelPickerFilter {
    All,
    Favorites,
    Provider(String),
}

#[derive(Default)]
struct AgentCatalogData {
    providers: Vec<agent_auth::Provider>,
    models: Vec<agent_auth::AgentModel>,
}

#[derive(Default)]
struct AgentCatalog {
    data: AgentCatalogData,
    loaded: bool,
    error: Option<String>,
    pending: Option<std::sync::mpsc::Receiver<Result<AgentCatalogData, String>>>,
    invalidated: bool,
}

impl AgentCatalog {
    fn provider_name(&self, id: &str) -> String {
        self.data
            .providers
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| provider_display_name(id))
    }

    fn refresh(&mut self) {
        if self.pending.is_some() {
            return;
        }
        self.error = None;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.pending = Some(receiver);
        std::thread::spawn(move || {
            load_provider_icons();
            let models = agent_auth::available_models();
            let providers = agent_auth::providers();
            let result = models.and_then(|models| {
                providers.map(|providers| AgentCatalogData { providers, models })
            });
            let _ = sender.send(result);
            crate::agent_bridge::signal_wake();
        });
    }

    fn invalidate(&mut self) {
        if self.pending.is_some() {
            self.invalidated = true;
        } else {
            self.refresh();
        }
    }

    fn poll(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let Some(receiver) = &self.pending else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("Model discovery stopped unexpectedly.".into()),
        };
        self.pending = None;
        if std::mem::take(&mut self.invalidated) {
            self.refresh();
        } else {
            match result {
                Ok(data) => {
                    *CUSTOM_PROVIDER_IDS.write() = data
                        .providers
                        .iter()
                        .filter(|provider| provider.custom)
                        .map(|provider| provider.id.clone())
                        .collect();
                    self.data = data;
                    self.loaded = true;
                }
                Err(error) => self.error = Some(error),
            }
        }
        true
    }
}

struct AuthModal {
    page: AgentDialogPage,
    model_filter: ModelPickerFilter,
    model_scroll: VirtualListScrollHandle,
    provider_scroll: VirtualListScrollHandle,
    focus: gpui::FocusHandle,
    cwd: PathBuf,
    methods_discovery: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    custom_provider: Option<[Entity<InputState>; 5]>,
    provider_operation: Option<std::sync::mpsc::Receiver<Result<Option<String>, String>>>,
    filter: Entity<InputState>,
    input: Entity<InputState>,
    login: Option<agent_auth::Login>,
    selected: Option<String>,
    output: String,
    error: Option<String>,
}

impl AuthModal {
    fn filter_models(&mut self, filter: ModelPickerFilter) {
        self.model_filter = if self.model_filter == filter {
            ModelPickerFilter::All
        } else {
            filter
        };
        self.model_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
    }

    fn discover_auth_methods(&mut self) {
        if self.methods_discovery.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.methods_discovery = Some(receiver);
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let _ = sender.send(embedded::refresh_auth_methods(&cwd));
            crate::agent_bridge::signal_wake();
        });
    }
}

type ModelBridge = agent_bridge::AgentBridge;

struct WorkerView {
    index: usize,
    id: String,
    conversation_key: String,
    messages: Vec<Arc<ChatMessage>>,
    expanded: HashSet<usize>,
    full_output: HashSet<usize>,
    expanded_tool_groups: HashMap<String, HashSet<usize>>,
    expanded_turns: HashMap<String, HashSet<usize>>,
    list_state: ListState,
    list_rows: std::cell::Cell<usize>,
    list_key: std::cell::Cell<u64>,
    row_cache: std::rc::Rc<std::cell::RefCell<ChatRowCache>>,
    active_dirty: std::cell::Cell<bool>,
    activity_dirty: std::cell::Cell<bool>,
    dirty_messages: std::cell::RefCell<HashSet<usize>>,
    _load: Option<gpui::Task<()>>,
}

struct ChatListSource<'a> {
    conversation_key: &'a str,
    messages: &'a [Arc<ChatMessage>],
    running: bool,
    list_state: &'a ListState,
    list_rows: &'a std::cell::Cell<usize>,
    list_key: &'a std::cell::Cell<u64>,
    row_cache: &'a std::rc::Rc<std::cell::RefCell<ChatRowCache>>,
    active_dirty: &'a std::cell::Cell<bool>,
    activity_dirty: &'a std::cell::Cell<bool>,
    dirty_messages: &'a std::cell::RefCell<HashSet<usize>>,
    expanded_tool_groups: &'a HashMap<String, HashSet<usize>>,
    expanded_turns: &'a HashMap<String, HashSet<usize>>,
}

/// Owned worker display data, refreshed only when worker activity changes.
#[derive(Clone)]
struct WorkerRowSnapshot {
    index: usize,
    id: String,
    avatar: usize,
    title: String,
    status: &'static str,
}

impl WorkerRowSnapshot {
    fn is_active(&self) -> bool {
        matches!(self.status, "is starting" | "is working")
    }
}

/// One virtualized transcript unit. Settled history stays measured and is not
/// rebuilt when the active reply grows.
enum ChatRow {
    Worked { start: usize, elapsed_ms: Option<u64>, open: bool, has_details: bool },
    Subagents {
        start: usize,
        rows: Vec<WorkerRowSnapshot>,
    },
    ToolGroup {
        start: usize,
        messages: Vec<Arc<ChatMessage>>,
        live: bool,
        open: bool,
    },
    Message {
        index: usize,
        message: Arc<ChatMessage>,
        live: bool,
        copy_range: Option<(usize, usize)>,
    },
    Running {
        label: SharedString,
        tooltip: SharedString,
    },
}

#[derive(Clone, Copy, Default)]
struct ChatRowCheckpoint {
    cursor: usize,
    turn_start: usize,
    last_assistant_row: Option<usize>,
    active_row: Option<usize>,
    worked: Option<WorkedRange>,
}

#[derive(Clone, Copy)]
struct WorkedRange { start: usize, end: usize, answer: Option<usize>, open: bool }

#[derive(Default)]
struct ChatRowCache {
    conversation: Option<String>,
    rows: Vec<ChatRow>,
    checkpoints: Vec<ChatRowCheckpoint>,
    state: ChatRowCheckpoint,
    message_count: usize,
    running: bool,
    tail: usize,
    dirty: bool,
    workers_dirty: bool,
    workers: Vec<WorkerRowSnapshot>,
    activity: Option<(SharedString, SharedString)>,
    #[cfg(test)]
    built_rows: usize,
}

impl ChatRowCache {
    fn list_item_count(&self) -> usize {
        self.rows.len()
            - usize::from(
                self.rows.len() > 1 && matches!(self.rows.last(), Some(ChatRow::Running { .. })),
            )
    }

    fn invalidate_from(&mut self, index: usize) {
        if self.dirty && index >= self.state.cursor {
            return;
        }
        let mut row = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.cursor <= index)
            .saturating_sub(1);
        while row > 0 && self.checkpoints[row - 1].cursor == self.checkpoints[row].cursor {
            row -= 1;
        }
        if let Some(checkpoint) = self.checkpoints.get(row).copied() {
            self.state = checkpoint;
            self.rows.truncate(row);
            self.checkpoints.truncate(row);
        }
        self.dirty = true;
    }

    fn invalidate_workers(&mut self) {
        self.workers_dirty = true;
        let first = self
            .workers
            .first()
            .map_or(self.state.cursor, |worker| worker.index);
        self.invalidate_from(first);
    }
}

/// Row storage is shared with list callbacks; invalidation drops mutable
/// message handles before the next event, avoiding copy-on-write of live text.
struct ChatSnapshot {
    focused_layout: bool,
    pane_id: usize,
    conversation: SharedString,
    tokens: ThemeTokens,
    rows: std::rc::Rc<std::cell::RefCell<ChatRowCache>>,
    unready_row: std::cell::Cell<Option<usize>>,
    list_state: Option<ListState>,
    preserve_anchor: bool,
    host: UiHost,
}
