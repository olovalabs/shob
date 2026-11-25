use theme::{PaletteScheme, ThemePreference, ThemeTokens};

/// Key context of the Shob input bar.
///
/// Tab has to be bound here rather than handled from `on_key_down`: the input
/// component binds `tab` to `IndentInline` and `gpui-component` binds it at the
/// `Root` context for focus traversal, which would otherwise consume the key
/// before the completion handler runs.
const SHOB_INPUT_CONTEXT: &str = "ShobInput";

actions!(shob_input, [CompleteCommand, IgnoreKey]);

fn init_shob_input_bindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("tab", CompleteCommand, Some(SHOB_INPUT_CONTEXT)),
        // Keep Shift+Tab from running the Root context's focus traversal.
        gpui::KeyBinding::new("shift-tab", IgnoreKey, Some(SHOB_INPUT_CONTEXT)),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ActiveView {
    #[default]
    Workspace,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsSection {
    #[default]
    Appearance,
    Data,
    Updates,
}

use assets::{CombinedAssets, MONO_FONT, load_embedded_fonts, sync_component_fonts};
use status::{SessionStatus, StatusInput};
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/icons/shob.png");
/// Default width of the navigation sidebar (Shob style).
const COMPACT_WINDOW_WIDTH: Pixels = px(768.0);
pub const SIDEBAR_WIDTH: Pixels = px(260.0);
/// Narrowest the sidebar can be dragged to; below this the session rows stop
/// being readable, so the edge refuses to go further.
pub const SIDEBAR_MIN_WIDTH: Pixels = px(180.0);
/// Widest the sidebar can be dragged to, as a fraction of the window width.
/// Keeps the terminal from being squeezed out on narrow windows.
const SIDEBAR_MAX_FRACTION: f32 = 0.5;
/// Width of the invisible drag strip along the sidebar's right border.
const SIDEBAR_RESIZE_HANDLE_WIDTH: Pixels = px(4.0);
/// T3 Code's row sizes and inbox paging.
const SIDEBAR_THREAD_CARD_HEIGHT: Pixels = px(78.0);
const SIDEBAR_THREAD_SLIM_HEIGHT: Pixels = px(36.0);
const SIDEBAR_ACTIVE_INITIAL_COUNT: usize = 5;
const SIDEBAR_SETTLED_INITIAL_COUNT: usize = 10;
const SIDEBAR_SETTLED_PAGE_COUNT: usize = 25;
const SIDEBAR_SNOOZE_MS: u64 = 24 * 60 * 60 * 1000;

/// A compact chevron wave, based on the Drive loader in the supplied design.
fn sidebar_loader(phase: f32, color: Hsla) -> Div {
    let mut grid = div().size(px(16.0)).flex().flex_col().gap(px(1.5));
    for row_index in 0i32..3 {
        let mut row = div().flex().gap(px(1.5));
        for column in 0i32..3 {
            let delay = (column + (row_index - 1).abs()) as f32 * 90.0 / 650.0;
            let cycle = (phase - delay).rem_euclid(1.0);
            let pulse = (std::f32::consts::PI * cycle).sin().powi(4);
            let opacity = 0.15 + 0.85 * pulse;
            row = row.child(
                div()
                    .size(px(4.0))
                    .rounded(px(1.0))
                    .bg(color)
                    .opacity(opacity),
            );
        }
        grid = grid.child(row);
    }
    grid
}

struct AgentActivityLoader(Hsla);

impl Render for AgentActivityLoader {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::{Animation, AnimationExt};

        let color = self.0;
        sidebar_loader(0.0, color).with_animation(
            "agent-loader-wave",
            Animation::new(std::time::Duration::from_millis(650))
                .repeat_synced()
                .with_max_fps(15.0),
            move |_, phase| sidebar_loader(phase, color),
        )
    }
}

fn animated_agent_loader(
    id: impl Into<ElementId>,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let loader = window.use_keyed_state(id, cx, |_, _| AgentActivityLoader(color));
    if loader.read(cx).0 != color {
        loader.update(cx, |loader, cx| {
            loader.0 = color;
            cx.notify();
        });
    }
    // Animation ticks invalidate the loader view, not its surrounding chat or drawer.
    loader.cached(StyleRefinement::default().size(px(16.0)).flex_shrink_0())
}

#[cfg(unix)]
static STATUS_BUS: std::sync::LazyLock<Option<status_bus::BusIdentity>> =
    std::sync::LazyLock::new(|| status_bus::start().ok());
#[cfg(unix)]
static LAST_STATUS_SNAPSHOT_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn status_snapshot_due() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let previous = LAST_STATUS_SNAPSHOT_MS.load(Ordering::Relaxed);
    now.saturating_sub(previous) >= 1000
        && LAST_STATUS_SNAPSHOT_MS
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Access to the owning app entity from a render closure that only receives a
/// plain `&mut App` (for example a virtualized list item).
#[derive(Clone)]
pub(crate) struct UiHost {
    app: gpui::WeakEntity<AppState>,
}

impl UiHost {
    pub(crate) fn new(app: gpui::WeakEntity<AppState>) -> Self {
        Self { app }
    }

    pub(crate) fn listener<E: ?Sized + 'static>(
        &self,
        handler: impl Fn(&mut AppState, &E, &mut Window, &mut gpui::Context<AppState>) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let app = self.app.clone();
        move |event, window, cx| {
            let _ = app.update(cx, |this, cx| handler(this, event, window, cx));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ChatRole {
    User,
    Assistant,
    Thinking,
    Status,
    Tool,
    Error,
}

#[derive(Clone)]
struct ChatMessage {
    role: ChatRole,
    text: String,
    tool: Option<ChatTool>,
    expanded: bool,
    /// Settled Markdown keeps its render cache; oversized live blocks use a
    /// bounded plain preview until the complete message is finalized.
    streaming: bool,
    /// Cached split refreshed when a delta arrives, never during rendering.
    /// Only the unfinished suffix needs boundary scanning.
    stream: Option<StreamView>,
    /// Shared, immutable completed document (also used by replayed messages).
    markdown: std::sync::OnceLock<MarkdownBlocks>,
    literal: std::sync::OnceLock<SharedString>,
    /// Metadata-only turn identity for opt-in paint timings.
    turn: Option<(u64, std::time::Instant)>,
    source_timestamp: Option<u64>,
    worked_ms: Option<u64>,
    worked_end: usize,
    worked_answer: Option<usize>,
}

#[derive(Clone)]
struct StreamView {
    settled: MarkdownBlocks,
    tail: SharedString,
    plain_tail: bool,
}

const STREAM_LIVE_BYTES: usize = 8 * 1024;

#[derive(Clone)]
struct ChatTool {
    workers: Vec<subagent_chat::Worker>,
    web_sources: Vec<String>,
    id: Option<String>,
    name: String,
    /// Shared so a frame can hand it to a card or a copy closure without
    /// copying a potentially megabyte-sized tool result.
    output: SharedString,
    /// Bounded view of `output`, computed when the output arrives. Rendering
    /// never scans or copies the full result unless the reader expands it.
    preview: SharedString,
    preview_truncated: bool,
    batch_result: Option<Arc<chat_batch::BatchResult>>,
    state: ToolState,
    expanded_calls: [bool; embedded::MAX_BATCH_CALLS],
    expanded: bool,
    /// Argument and output derivatives computed once when the tool starts.
    /// Rendering must not re-parse JSON or re-serialize arguments per frame.
    display: ToolDisplay,
    /// Oversized output is shown as a bounded preview until the reader expands
    /// it. Copy actions always use the complete received output.
    show_full: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ToolState {
    Running,
    Completed,
    Failed,
    Unknown,
}
impl ChatMessage {
    fn new(role: ChatRole, text: String) -> Self {
        Self {
            role,
            text,
            tool: None,
            expanded: role != ChatRole::Thinking,
            streaming: false,
            stream: None,
            markdown: std::sync::OnceLock::new(),
            literal: std::sync::OnceLock::new(),
            turn: None,
            source_timestamp: None,
            worked_ms: None,
            worked_end: 0,
            worked_answer: None,
        }
    }

    fn markdown(&self) -> MarkdownBlocks {
        self.markdown
            .get_or_init(|| {
                let document = MarkdownBlocks::default();
                document.finish(SharedString::from(self.text.clone()));
                document
            })
            .clone()
    }
}

fn fence_open(line: &str) -> Option<(u8, usize)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let marker = *line.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|b| *b == marker).count();
    if len < 3 {
        return None;
    }
    if marker == b'`' && line[len..].contains('`') {
        return None;
    }
    Some((marker, len))
}

fn fence_close(line: &str, marker: u8, len: usize) -> bool {
    let line = line.trim_end_matches(['\r', '\n']).trim_end();
    let count = line.bytes().take_while(|b| *b == marker).count();
    count >= len && line[count..].trim().is_empty()
}

fn list_marker_len(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    match bytes.first()? {
        b'-' | b'*' | b'+' if bytes.get(1) == Some(&b' ') => Some(1),
        b'0'..=b'9' => {
            let mut digits = 0;
            while bytes.get(digits).is_some_and(u8::is_ascii_digit) {
                digits += 1;
            }
            if (1..=9).contains(&digits)
                && matches!(bytes.get(digits), Some(b'.') | Some(b')'))
                && bytes.get(digits + 1) == Some(&b' ')
            {
                Some(digits + 1)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Never partition a fence or list: nested/lazy list continuations can depend on
/// earlier items, so a list and everything after it stays live until completion.
/// Callers bound this scan and switch oversized live content to a plain preview.
fn split_streaming_markdown(text: &str) -> (&str, &str) {
    let mut fence: Option<(u8, usize)> = None;
    let mut boundary: Option<usize> = None;
    let mut last_blank: Option<usize> = None;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();

        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();

        if let Some((marker, len)) = fence {
            if indent <= 3 && fence_close(trimmed, marker, len) {
                fence = None;
            }
            continue;
        }

        if indent <= 3
            && let Some(opened) = fence_open(trimmed)
        {
            boundary = last_blank.or(boundary);
            fence = Some(opened);
            continue;
        }

        if line.trim().is_empty() {
            last_blank = Some(start);
            boundary = Some(start);
            continue;
        }

        if indent <= 3 && list_marker_len(trimmed).is_some() {
            let at = boundary.or(last_blank).unwrap_or(0);
            return (&text[..at], text[at..].trim_start_matches(['\r', '\n']));
        }
        boundary = last_blank.or(boundary);
    }

    let at = boundary;
    match at {
        Some(at) => (&text[..at], text[at..].trim_start_matches(['\r', '\n'])),
        None => ("", text),
    }
}

/// Unicode-safe clipping: never slices through a multi-byte character.
fn clip_tool_text(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let mut shown: String = chars.by_ref().take(limit).collect();
    let truncated = chars.next().is_some();
    if truncated {
        shown.push('…');
    }
    (shown, truncated)
}

fn format_tool_summary(_name: &str, input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return String::new();
    }
    let clip = |text: &str| clip_tool_text(text, 60).0;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input)
        && let Some(map) = value.as_object()
    {
        if let Some(cmd) = map.get("command").and_then(|v| v.as_str()) {
            let first_line = cmd.lines().next().unwrap_or("").trim();
            return format!("$ {}", clip(first_line));
        }
        if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
            if let Some(pattern) = map.get("pattern").and_then(|v| v.as_str()) {
                return format!("{path} · {pattern}");
            }
            return path.to_string();
        }
        if let Some(pattern) = map.get("pattern").and_then(|v| v.as_str()) {
            return format!("/{pattern}/");
        }
        if let Some(query) = map.get("query").and_then(|v| v.as_str()) {
            return format!("\"{query}\"");
        }
        if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
            return url.to_string();
        }
        let mut parts = Vec::new();
        for (k, v) in map.iter().take(2) {
            let val_str = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => serde_json::to_string(v).unwrap_or_default(),
            };
            parts.push(format!("{k}: {val_str}"));
        }
        if !parts.is_empty() {
            return clip(&parts.join(", "));
        }
    }
    clip(input.lines().next().unwrap_or("").trim())
}

/// Shared assistant/reasoning typography; prompts and tool output stay literal.
enum ChatMarkdown {
    Text(SharedString),
    Blocks(MarkdownBlocks),
}

impl From<SharedString> for ChatMarkdown {
    fn from(text: SharedString) -> Self {
        Self::Text(text)
    }
}
impl From<String> for ChatMarkdown {
    fn from(text: String) -> Self {
        Self::Text(text.into())
    }
}
impl From<MarkdownBlocks> for ChatMarkdown {
    fn from(blocks: MarkdownBlocks) -> Self {
        Self::Blocks(blocks)
    }
}

struct ChatRenderContext<'a> {
    tokens: ThemeTokens,
    window: &'a mut Window,
    cx: &'a mut App,
}

struct ChatMessageOptions<'a> {
    pane_id: usize,
    conversation: &'a str,
    index: usize,
    running: bool,
    copy_range: Option<(usize, usize)>,
}

fn chat_markdown(
    id: SharedString,
    text: impl Into<ChatMarkdown>,
    base: Pixels,
    paragraph_gap: Rems,
    color: Hsla,
    render: ChatRenderContext<'_>,
) -> TextView {
    let ChatRenderContext { tokens, window, cx } = render;
    let dark = cx.theme().is_dark();
    let themed_id: SharedString = format!("{id}-{}", if dark { "dark" } else { "light" }).into();

    match text.into() {
        ChatMarkdown::Text(text) => TextView::markdown(themed_id, text, window, cx),
        ChatMarkdown::Blocks(blocks) => TextView::markdown_blocks(themed_id, blocks, window, cx),
    }
    .style(
        tokens.chat().markdown(dark, false, cx.theme().highlight_theme.clone())
            .heading_font_size(move |level, _| base * match level {
                1 => 20.0 / 14.0,
                2 => 18.0 / 14.0,
                3 => 16.0 / 14.0,
                _ => 1.0,
            })
            .paragraph_gap(paragraph_gap),
    )
    .code_block_actions(move |block, _window, _cx| {
        let code = block.code();
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(6.0))
            .text_size(px(11.0))
            .text_color(tokens.chat().muted)
            .when_some(block.lang(), |actions, lang| actions.child(lang))
            .child(
                div().debug_selector(|| "chat-code-copy".to_string()).child(
                    Button::new("chat-code-copy")
                        .icon(gpui_component::IconName::Copy)
                        .small()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()));
                        })
                ),
            )
            .into_any_element()
    })
    .selectable(true)
    .w_full()
    .text_size(base)
    .text_color(color)
}

fn chat_stream_tail(
    id: SharedString,
    view: &StreamView,
    base: Pixels,
    paragraph_gap: Rems,
    color: Hsla,
    render: ChatRenderContext<'_>,
) -> AnyElement {
    let ChatRenderContext { tokens, window, cx } = render;
    if !view.plain_tail {
        return chat_markdown(
            id,
            view.tail.clone(),
            base,
            paragraph_gap,
            color,
            ChatRenderContext { tokens, window, cx },
        )
        .into_any_element();
    }
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .debug_selector(|| "stream-live-preview".to_string())
        .child(
            div()
                .text_size(px(11.0))
                .text_color(tokens.text_muted)
                .child("Earlier live content is temporarily hidden. Full formatting returns when this message completes."),
        )
        .child(
            div()
                .w_full()
                .whitespace_normal()
                .font_family(MONO_FONT)
                .text_size(base)
                .text_color(color)
                .child(view.tail.clone()),
        )
        .into_any_element()
}

fn chat_stream_copy(
    host: &UiHost,
    pane_id: usize,
    index: usize,
    tokens: ThemeTokens,
) -> impl IntoElement {
    div()
        .id(SharedString::from(format!(
            "chat-copy-live-{pane_id}-{index}"
        )))
        .debug_selector(|| "stream-copy-full-message".to_string())
        .text_size(px(11.0))
        .text_color(tokens.text_muted)
        .cursor(CursorStyle::PointingHand)
        .on_click(host.listener(move |this, _, _, cx| {
            if let Some(message) = this
                .pane_ref(pane_id)
                .and_then(|pane| pane.chat.visible_messages().get(index))
            {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(message.text.clone()));
            }
        }))
        .child("Copy full message")
}

/// Capitalize the first character of a tool or agent name.
fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn is_agent_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "task"
            | "subagent"
            | "agent"
            | "delegate"
            | "delegate_task"
            | "spawn_agent"
            | "spawn_subagent"
    )
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "shell"
            | "bash"
            | "powershell"
            | "run"
            | "run_command"
            | "exec"
            | "execute"
            | "terminal"
            | "command"
    )
}

fn tool_title(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "read" | "read_file" => "Read".into(),
        "list" | "ls" | "list_dir" => "List".into(),
        "find" | "glob" => "Glob".into(),
        "grep" | "rg" => "Ripgrep".into(),
        "ast_grep" | "search" => "Search".into(),
        "webfetch" | "web_fetch" | "fetch" => "Webfetch".into(),
        "websearch" | "web_search" | "search_web" => "Web Search".into(),
        "shell" | "bash" | "powershell" | "run" | "run_command" | "exec" | "execute"
        | "terminal" | "command" => "Shell".into(),
        "edit" | "str_replace" | "str_replace_editor" | "apply_patch" => "Edit".into(),
        "write" | "write_file" | "create_file" => "Write".into(),
        "patch" => "Patch".into(),
        "task" | "subagent" | "agent" | "delegate" | "delegate_task" | "spawn_agent"
        | "spawn_subagent" => "Agent".into(),
        "question" | "ask" | "questions" => "Questions".into(),
        "skill" => "Skill".into(),
        "todowrite" | "todo_write" | "todos" => "Todos".into(),
        "browser" => "Browser".into(),
        "batch" => "Batch".into(),
        _ => title_case(name),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[derive(Clone)]
struct ToolView {
    title: String,
    subtitle: String,
    args: Vec<SharedString>,
}

/// Parsed, render-ready tool fields. Built once per tool call so a stream of
/// renders does not re-parse the same JSON arguments or output.
#[derive(Clone)]
struct ToolDisplay {
    view: ToolView,
    batch_calls: chat_batch::BatchCalls,
    activity_detail: Option<String>,
    shell_command: Option<SharedString>,
    shell_preview: Option<SharedString>,
    shell_truncated: bool,
    show_raw_input: bool,
    input: SharedString,
    input_preview: SharedString,
    input_truncated: bool,
    content_body: Option<SharedString>,
    content_preview: Option<SharedString>,
    content_truncated: bool,
}

impl ToolDisplay {
    fn new(name: &str, input: &str) -> Self {
        let parsed = serde_json::from_str::<serde_json::Value>(input.trim()).ok();
        let (input, input_preview, input_truncated) = tool_output_fields(input.to_owned());
        let content_body = tool_content_body(name, parsed.as_ref()).map(SharedString::from);
        let (content_preview, content_truncated) = match content_body.as_ref() {
            Some(body) => {
                let (end, truncated) = output_preview_len(body);
                (
                    Some(if truncated {
                        SharedString::from(&body[..end])
                    } else {
                        body.clone()
                    }),
                    truncated,
                )
            }
            None => (None, false),
        };
        let view = tool_view(name, &input, parsed.as_ref());
        let shell = is_shell_tool(name)
            .then(|| shell_command(&input, parsed.as_ref()))
            .filter(|command| !command.is_empty());
        let (shell_command, shell_preview, shell_truncated) = match shell {
            Some(command) => {
                let (full, preview, truncated) = tool_output_fields(command);
                (Some(full), Some(preview), truncated)
            }
            None => (None, None, false),
        };
        let show_raw_input = content_body.is_none()
            && shell_command
                .as_ref()
                .is_none_or(|command| command.is_empty())
            && (view.subtitle.is_empty()
                || !matches!(
                    view.title.as_str(),
                    "Read" | "List" | "Glob" | "Ripgrep" | "Search" | "Webfetch" | "Web Search"
                ));
        Self {
            view,
            batch_calls: if chat_batch::is_batch(name) {
                chat_batch::calls(parsed.as_ref())
            } else {
                chat_batch::BatchCalls::default()
            },
            activity_detail: parsed.as_ref().and_then(|value| {
                ["path", "file_path", "filePath"]
                    .into_iter()
                    .find_map(|key| value.get(key).and_then(|value| value.as_str()))
                    .filter(|path| !path.is_empty())
                    .map(|path| clip_tool_text(first_line(path), 300).0)
            }),
            shell_command,
            shell_preview,
            shell_truncated,
            show_raw_input,
            input,
            input_preview,
            input_truncated,
            content_body,
            content_preview,
            content_truncated,
        }
    }
}

/// Parse the compact tool summary and its expanded arguments.
fn tool_view(name: &str, input: &str, parsed: Option<&serde_json::Value>) -> ToolView {
    let object = parsed.and_then(|value| value.as_object());
    let field = |key: &str| {
        object
            .and_then(|map| map.get(key))
            .and_then(|value| value.as_str())
    };
    let clip = |text: &str, limit: usize| clip_tool_text(text, limit).0;

    let lower = name.to_ascii_lowercase();
    let mut title = tool_title(name);
    let mut subtitle = String::new();
    let mut args = Vec::new();

    if is_agent_tool(name) {
        if let Some(agent) = field("agent")
            .or_else(|| field("role"))
            .or_else(|| field("subagent_type"))
            .or_else(|| field("subagentType"))
            .or_else(|| field("type"))
        {
            title = format!("{} Agent", title_case(agent));
        }
        subtitle = field("description")
            .or_else(|| field("task"))
            .or_else(|| field("prompt"))
            .map(|value| clip(first_line(value), 80))
            .unwrap_or_default();
    } else if is_shell_tool(name) {
        subtitle = field("description")
            .or_else(|| field("title"))
            .or_else(|| {
                parsed
                    .and_then(|value| value.get("metadata"))
                    .and_then(|metadata| metadata.get("title"))
                    .and_then(serde_json::Value::as_str)
            })
            .filter(|text| !text.trim().is_empty())
            .map(first_line)
            .map(str::to_owned)
            .unwrap_or_else(|| first_line(&shell_command(input, parsed)).to_owned());
    } else {
        match lower.as_str() {
            "read" | "read_file" => {
                if let Some(path) = field("path")
                    .or_else(|| field("file_path"))
                    .or_else(|| field("filePath"))
                {
                    subtitle = file_name(path).to_string();
                }
                if let Some(offset) = object.and_then(|map| map.get("offset")) {
                    args.push(format!("offset={offset}"));
                }
                if let Some(limit) = object.and_then(|map| map.get("limit")) {
                    args.push(format!("limit={limit}"));
                }
            }
            "list" | "ls" | "list_dir" => {
                subtitle = field("path").unwrap_or(".").to_string();
            }
            "find" | "glob" | "grep" | "ast_grep" | "search" | "rg" => {
                subtitle = field("path").unwrap_or(".").to_owned();
                for key in ["pattern", "include"] {
                    if let Some(value) = object.and_then(|map| map.get(key)) {
                        let value = value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_owned);
                        args.push(format!("{key}={value}"));
                    }
                }
            }
            "webfetch" | "web_fetch" | "fetch" => {
                subtitle = field("url").unwrap_or_default().to_string();
            }
            "websearch" | "web_search" | "search_web" => {
                subtitle = field("query").unwrap_or_default().to_string();
            }
            "edit" | "str_replace" | "str_replace_editor" | "apply_patch" | "write"
            | "write_file" | "create_file" | "patch" => {
                if let Some(path) = field("path")
                    .or_else(|| field("file_path"))
                    .or_else(|| field("filePath"))
                {
                    subtitle = file_name(path).to_string();
                }
            }
            "batch" => {}
            _ => {
                subtitle = format_tool_summary(name, input);
            }
        }
    }

    ToolView {
        title,
        subtitle: clip(first_line(&subtitle), 300),
        args: args
            .into_iter()
            .map(|arg| SharedString::from(clip(first_line(&arg), 300)))
            .collect(),
    }
}

fn chat_copy_button(
    id: SharedString,
    text: SharedString,
    tip: SharedString,
    tokens: ThemeTokens,
) -> impl IntoElement {
    let keyboard_text = text.clone();
    div()
        .id(id)
        .tab_index(0)
        .size(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .cursor(CursorStyle::PointingHand)
        .text_color(tokens.text_muted)
        .hover(move |style| {
            style
                .bg(tokens.sidebar_item_hover)
                .text_color(tokens.text_primary)
        })
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .on_key_down(move |event, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(keyboard_text.to_string()));
            }
        })
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
        })
        .child(
            svg()
                .path("icons/copy.svg")
                .size(px(12.0))
                .text_color(tokens.text_muted),
        )
}

const TOOL_OUTPUT_PREVIEW_BYTES: usize = 16 * 1024;
const TOOL_OUTPUT_PREVIEW_LINES: usize = 400;

/// Byte length of the bounded view of oversized tool output, and whether
/// content was omitted. Computed when output changes, never during a render.
fn output_preview_len(text: &str) -> (usize, bool) {
    if text.len() <= TOOL_OUTPUT_PREVIEW_BYTES
        && text.bytes().filter(|byte| *byte == b'\n').count() <= TOOL_OUTPUT_PREVIEW_LINES
    {
        return (text.len(), false);
    }
    let mut end = text.len().min(TOOL_OUTPUT_PREVIEW_BYTES);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut lines = 0;
    let mut line_end = 0;
    for (index, byte) in text.as_bytes().iter().enumerate().take(end) {
        if *byte == b'\n' {
            lines += 1;
            if lines >= TOOL_OUTPUT_PREVIEW_LINES {
                line_end = index + 1;
                break;
            }
        }
    }
    if line_end > 0 && line_end < end {
        end = line_end;
    }
    (end, true)
}

/// Full and bounded shared views of one tool result, built when it arrives.
fn tool_output_fields(output: String) -> (SharedString, SharedString, bool) {
    let (end, truncated) = output_preview_len(&output);
    let full = SharedString::from(output);
    let preview = if truncated {
        SharedString::from(&full[..end])
    } else {
        full.clone()
    };
    (full, preview, truncated)
}
