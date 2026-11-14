//! AI-authored JSON UI (json-render style) for chat messages.
//!
//! The model emits a fenced ` ```ui ` block containing a flat element map.
//! The chat renderer recognizes the fence and draws gpui-kit components
//! instead of showing the JSON. Only the catalog below is reachable, so the
//! model cannot draw arbitrary UI.

use std::collections::{HashMap, HashSet};

mod numeric;
mod selection;
mod streaming;
mod theme;
pub(crate) use theme::sync as sync_theme;

use gpui::{
    AnyElement, Hsla, InteractiveElement as _, IntoElement, OperatingSystem, ParentElement, Pixels,
    Rems, SharedString, Styled, Window, div, prelude::FluentBuilder, px, relative,
};
use gpui_component::input::{Input, InputState};
use gpui_kit::component::{
    Disableable as _, Icon, IconName, Sizable as _, Size,
    accordion::Accordion,
    alert::Alert,
    avatar::Avatar,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _, Toggle},
    chart::{BarChart, LineChart, PieChart},
    checkbox::Checkbox,
    description_list::{DescriptionItem, DescriptionList},
    group_box::{GroupBox, GroupBoxVariants as _},
    kbd::Kbd,
    marker::{Marker, MarkerContent},
    progress::Progress,
    radio::RadioGroup,
    rating::Rating,
    separator::Separator,
    skeleton::Skeleton,
    spinner::Spinner,
    stepper::{Stepper, StepperItem},
    switch::Switch,
    tab::{Tab, TabBar},
    table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow},
    tag::{Tag, TagVariant},
    text::TextView,
};

use crate::AppState;
use crate::assets::MONO_FONT;
use crate::theme::ThemeTokens;

const MAX_NODES: usize = 300;
const MAX_DEPTH: usize = 24;
const MAX_SPEC_BYTES: usize = 256 * 1024;

pub(crate) const CATALOG_PROMPT: &str = r#"## Kathamo UI cards

When a visual, chart, comparison, or dashboard would help the user, render it
inline using the format below. Use the components creatively for the user's
data and goals; do not invent measurements or imply interactions that are not
provided. Explain any important limitations in the surrounding text.
The fence must contain a single valid JSON object. Do not use YAML,
component names as keys (such as `Card:`), or a nested code fence.

```ui
{
  "root": "card",
  "elements": {
    "card": {"type":"Card","props":{"title":"System performance"},"children":["status","storage","metrics","usage"]},
    "status": {"type":"Tag","props":{"text":"Operational","tone":"success"}},
    "storage": {"type":"Progress","props":{"value":65,"label":"Storage used"}},
    "metrics": {"type":"Table","props":{"columns":["Metric","Value"],"rows":[["CPU","42%"],["Memory","58%"],["Storage","65%"]]}},
    "usage": {"type":"Chart","props":{"kind":"bar","height":140,"data":[{"label":"CPU","value":42},{"label":"Memory","value":58},{"label":"Storage","value":65}]}}
  }
}
```

Rules:
- Emit one JSON object with `root` (an element id) and `elements` (id -> node).
- A node is `{"type": "...", "props": {...}, "children": ["id", ...]}`.
- Only the component types listed below are allowed. Unknown types render an
  error chip. Prefer 1-3 cards per answer, not the whole conversation.
- Checkbox, Switch, Toggle, RadioGroup, Rating, Tabs, Stepper, Pagination,
  Accordion and Collapsible keep their own state when the reader clicks them.
  Button only acts when `action` is set; that text is sent as the next prompt.
- Use `children` for nesting. Keep ids short and unique within the block.
- Do not put HTML, CSS, or scripts in any prop.
- Text props accept Markdown unless stated otherwise.

Components and props:
- Card: title?, footer?, variant? ("normal"|"fill"|"outline"), children
- Row: gap?, align? ("start"|"center"|"end"|"stretch"), justify? ("start"|"center"|"end"|"between"), wrap?, children
- Column: gap?, align?, justify?, children
- Grid: gap?, cols? (1-6), children
- Heading: text, level? (1-4)
- Text: text, muted?, markdown? (default true)
- Input: label?, placeholder?, value?, disabled?, multiline? (editable locally)
- PasswordInput: label?, placeholder?, value?, disabled? (editable locally)
- NumberInput: label?, value?, min?, max?, step?, disabled? (editable locally; bounds apply when stepping)
- Slider: label?, value?, min?, max?, step?, disabled? (editable locally)
- Select: options (array of strings), selected? (index), label?, placeholder?, searchable?, disabled? (selection is local)
- Field: label?, description?, children
- Label: text
- Meter: value, min?, max?, label?
- SegmentedControl: options (array of strings), selected? (index), children? (one panel per option)
- Toolbar: gap?, children
- Carousel: selected? (index), children (slides; previous/next buttons)
- Timeline: items (array of {title, description?})
- Code: text, lang?
- Divider: label?
- Tag: text, tone? ("primary"|"secondary"|"success"|"warning"|"danger"|"info"), outline?
- Alert: message, title?, tone? ("info"|"success"|"warning"|"error")
- Progress: value (0-100), label?, loading?
- Spinner: size? ("xs"|"sm"|"md"|"lg")
- Skeleton: width?, height?
- Avatar: name?, src? (image URL or path)
- Icon: name (see list), size? ("xs"|"sm"|"md"|"lg")
- Kbd: keys (e.g. "cmd-k", "ctrl-shift-p")
- DescriptionList: items (array of {label, value}), columns? (1-10), bordered?
- Table: columns (array of strings), rows (array of string arrays), caption?
- Button: label, tone? ("primary"|"secondary"|"success"|"warning"|"danger"|"info"|"ghost"), outline?, action? (prompt text sent when clicked), disabled?
- Checkbox: label, checked?
- Switch: label, checked?
- RadioGroup: options (array of strings), selected? (index), horizontal?
- Rating: value? (0-5), max? (default 5)
- Stepper: steps (array of strings), selected? (index)
- Toggle: label, checked?
- Tabs: tabs (array of strings), selected? (index)
- Breadcrumb: items (array of strings)
- Marker: text
- Accordion: items (array of {title, content, open?}), multiple?
- Collapsible: title, content, open?
- Pagination: page?, total?
- Chart: kind ("line"|"bar"|"pie"), data (array of {label, value}), height? (px)
- Metrics: items (array of {label, value})
- Heatmap: columns (array of labels), rows (array of {label, values: [numbers]}), min?, max?; useful for activity by day and hour
- StackedBar: series (array of labels), rows (array of {label, values: [nonnegative numbers]}); useful for proportional comparisons
- List: items (array of strings), ordered?
- Diff: patch (unified diff text)
- Link: text, href
- Bubble: text, role? ("user"|"assistant")

Icon names: info, check, warning, error, search, settings, file, folder,
terminal, code, play, pause, plus, close, copy, external-link, calendar, clock,
bell, star, heart, trash, download, upload, zap, sparkles, bot, chart-pie,
globe, github, user, loader."#;

/// One assistant-text segment: plain Markdown, or a `ui` fence.
pub(crate) enum Segment<'a> {
    Markdown(&'a str),
    Ui { raw: &'a str, closed: bool },
}

/// Host-owned state for one interactive element in a rendered card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiStateValue {
    Bool(bool),
    Index(usize),
    Indices(Vec<usize>),
}

/// Split assistant text into Markdown and `ui` code fences.
pub(crate) fn split_ui_segments(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut markdown_start = 0usize;
    let mut offset = 0usize;
    let mut open: Option<(u8, usize, usize)> = None;
    let mut markdown_fence: Option<(u8, usize)> = None;

    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();

        if let Some((marker, len, content_start)) = open {
            if indent <= 3 && crate::fence_close(trimmed, marker, len) {
                segments.push(Segment::Ui {
                    raw: &text[content_start..start],
                    closed: true,
                });
                markdown_start = offset;
                open = None;
            }
            continue;
        }

        // A UI-looking fence inside a regular code block is literal code.
        // Track the outer fence so it cannot turn an example into a live card.
        if let Some((marker, len)) = markdown_fence {
            if indent <= 3 && crate::fence_close(trimmed, marker, len) {
                markdown_fence = None;
            }
            continue;
        }

        if indent <= 3
            && let Some((marker, len)) = crate::fence_open(trimmed)
        {
            let info = trimmed[len..].trim().to_ascii_lowercase();
            let info = info.split_whitespace().next().unwrap_or("");
            if matches!(info, "ui" | "json-ui" | "ui-spec") {
                if markdown_start < start {
                    segments.push(Segment::Markdown(&text[markdown_start..start]));
                }
                markdown_start = offset;
                open = Some((marker, len, offset));
            } else {
                markdown_fence = Some((marker, len));
            }
        }
    }

    match open {
        Some((_, _, content_start)) => {
            segments.push(Segment::Ui {
                raw: &text[content_start..],
                closed: false,
            });
        }
        None => {
            if markdown_start < text.len() {
                segments.push(Segment::Markdown(&text[markdown_start..]));
            }
        }
    }
    segments
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiSpec {
    root: String,
    #[serde(default)]
    elements: HashMap<String, UiNode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiNode {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    props: serde_json::Value,
    #[serde(default)]
    children: Vec<String>,
}

fn parse_spec(raw: &str) -> Result<UiSpec, String> {
    if raw.len() > MAX_SPEC_BYTES {
        return Err(format!("UI spec is too large ({} bytes)", raw.len()));
    }
    let spec: UiSpec = serde_json::from_str(raw.trim()).map_err(|error| {
        format!("Expected a JSON object with `root` and `elements` in the UI fence. {error}")
    })?;
    if spec.elements.len() > MAX_NODES {
        return Err(format!(
            "UI spec has {} elements; the limit is {MAX_NODES}",
            spec.elements.len()
        ));
    }
    if !spec.elements.contains_key(&spec.root) {
        return Err(format!("root `{}` is not in elements", spec.root));
    }
    let mut seen = HashSet::new();
    let mut visited = 0;
    check_cycle(&spec, &spec.root, 0, &mut seen, &mut visited)?;
    Ok(spec)
}

fn check_cycle(
    spec: &UiSpec,
    id: &str,
    depth: usize,
    seen: &mut HashSet<String>,
    visited: &mut usize,
) -> Result<(), String> {
    *visited += 1;
    if *visited > MAX_NODES {
        return Err(format!("UI expands to more than {MAX_NODES} elements"));
    }
    if depth > MAX_DEPTH {
        return Err(format!("UI nesting is deeper than {MAX_DEPTH}"));
    }
    if !seen.insert(id.to_string()) {
        return Err(format!("cycle in element graph at `{id}`"));
    }
    if let Some(node) = spec.elements.get(id) {
        for child in &node.children {
            if !spec.elements.contains_key(child) {
                return Err(format!("element `{id}` references missing child `{child}`"));
            }
            check_cycle(spec, child, depth + 1, seen, visited)?;
        }
    }
    seen.remove(id);
    Ok(())
}

struct UiCtx<'a> {
    spec: &'a UiSpec,
    id_prefix: &'a str,
    pane_id: usize,
    tokens: ThemeTokens,
    state: Option<&'a HashMap<String, UiStateValue>>,
}

fn state_key(ctx: &UiCtx, id: &str) -> String {
    format!("{}-{id}", ctx.id_prefix)
}

fn state_bool(ctx: &UiCtx, key: &str, default: bool) -> bool {
    match ctx.state.and_then(|state| state.get(key)) {
        Some(UiStateValue::Bool(value)) => *value,
        _ => default,
    }
}

fn state_index(ctx: &UiCtx, key: &str, default: usize) -> usize {
    match ctx.state.and_then(|state| state.get(key)) {
        Some(UiStateValue::Index(value)) => *value,
        _ => default,
    }
}

fn state_indices(ctx: &UiCtx, key: &str, default: Vec<usize>) -> Vec<usize> {
    match ctx.state.and_then(|state| state.get(key)) {
        Some(UiStateValue::Indices(value)) => value.clone(),
        _ => default,
    }
}

enum PreparedSegment {
    Markdown(SharedString),
    PendingUi,
    Ui(Result<std::sync::Arc<UiSpec>, (String, SharedString)>),
}

#[derive(Default)]
struct AssistantBody {
    source: SharedString,
    segments: std::sync::Arc<Vec<Option<PreparedSegment>>>,
    pending: HashMap<usize, (usize, Option<std::sync::Arc<UiSpec>>)>,
}

impl AssistantBody {
    fn prepare(&mut self, text: &str) {
        if !text.starts_with(self.source.as_ref()) {
            self.pending.clear();
        }
        self.segments = std::sync::Arc::new(
            split_ui_segments(text)
                .into_iter()
                .enumerate()
                .map(|(index, segment)| match segment {
                    Segment::Markdown(markdown) if !markdown.trim().is_empty() => {
                        Some(PreparedSegment::Markdown(SharedString::from(markdown)))
                    }
                    Segment::Ui { raw, closed: false } => {
                        // A streamed JSON object is often complete before its closing
                        // Markdown fence arrives. Show it immediately when valid;
                        // incomplete or invalid objects remain pending until closure.
                        let cached = self.pending.entry(index).or_default();
                        let appended = raw.get(cached.0..).unwrap_or(raw);
                        if cached.0 > raw.len() || appended.contains('}') {
                            cached.1 = streaming::partial_spec(raw).map(std::sync::Arc::new);
                        }
                        cached.0 = raw.len();
                        match cached.1.clone() {
                            Some(spec) => Some(PreparedSegment::Ui(Ok(spec))),
                            None => Some(PreparedSegment::PendingUi),
                        }
                    }
                    Segment::Ui { raw, closed: true } => {
                        self.pending.remove(&index);
                        Some(PreparedSegment::Ui(
                            parse_spec(raw)
                                .map(std::sync::Arc::new)
                                .map_err(|error| (error, SharedString::from(raw))),
                        ))
                    }
                    _ => None,
                })
                .collect(),
        );
        self.source = SharedString::from(text);
    }
}

/// Render an assistant message body, replacing ` ```ui ` fences with cards.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_assistant_body(
    id: SharedString,
    text: &str,
    base: Pixels,
    paragraph_gap: Rems,
    color: Hsla,
    tokens: ThemeTokens,
    pane_id: usize,
    state: Option<&HashMap<String, UiStateValue>>,
    host: &crate::UiHost,
    window: &mut Window,
    cx: &mut gpui::App,
) -> AnyElement {
    let body = window.use_keyed_state(
        SharedString::from(format!("{id}-assistant-body")),
        cx,
        |_, _| AssistantBody::default(),
    );
    if body.read(cx).source.as_ref() != text {
        body.update(cx, |body, _| body.prepare(text));
    }
    let segments = body.read(cx).segments.clone();
    let mut column = div().w_full().min_w_0().flex().flex_col().gap(px(8.0));
    for (ix, segment) in segments.iter().enumerate() {
        match segment {
            Some(PreparedSegment::Markdown(markdown)) => {
                column = column.child(crate::chat_markdown(
                    SharedString::from(format!("{id}-md-{ix}")),
                    markdown.clone(),
                    base,
                    paragraph_gap,
                    color,
                    tokens,
                    window,
                    cx,
                ));
            }
            Some(segment) => {
                column = column.child(render_ui_block(
                    SharedString::from(format!("{id}-ui-{ix}")),
                    segment,
                    tokens,
                    pane_id,
                    state,
                    host,
                    window,
                    cx,
                ));
            }
            None => {}
        }
    }
    column.into_any_element()
}

fn render_ui_block(
    id: SharedString,
    segment: &PreparedSegment,
    tokens: ThemeTokens,
    pane_id: usize,
    state: Option<&HashMap<String, UiStateValue>>,
    host: &crate::UiHost,
    window: &mut Window,
    cx: &mut gpui::App,
) -> AnyElement {
    let debug_id = id.clone();
    let chrome = |inner: AnyElement| {
        div()
            .debug_selector(move || format!("ai-ui-block-{debug_id}"))
            .w_full()
            .min_w_0()
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.border_subtle)
            .bg(tokens.card_bg)
            .text_color(tokens.text_primary)
            .p(px(12.0))
            .child(inner)
            .into_any_element()
    };

    let PreparedSegment::Ui(spec) = segment else {
        return chrome(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(12.0))
                .text_color(tokens.text_muted)
                .child(Spinner::new().with_size(Size::Small))
                .child("Rendering UI…")
                .into_any_element(),
        );
    };

    match spec {
        Ok(spec) => {
            let ctx = UiCtx {
                spec,
                id_prefix: id.as_ref(),
                pane_id,
                tokens,
                state,
            };
            let content = render_node(&ctx, &spec.root, 0, host, window, cx);
            chrome(content)
        }
        Err((error, raw)) => chrome(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    Alert::error(
                        SharedString::from(format!("{id}-spec-error")),
                        error.clone(),
                    )
                    .title("Invalid UI spec"),
                )
                .child(
                    div()
                        .font_family(MONO_FONT)
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(raw.clone()),
                )
                .into_any_element(),
        ),
    }
}

fn prop_str(props: &serde_json::Value, key: &str) -> Option<String> {
    props.get(key).and_then(|value| match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    })
}

fn prop_bool(props: &serde_json::Value, key: &str) -> Option<bool> {
    props.get(key).and_then(serde_json::Value::as_bool)
}

fn prop_f64(props: &serde_json::Value, key: &str) -> Option<f64> {
    props.get(key).and_then(serde_json::Value::as_f64)
}

fn prop_usize(props: &serde_json::Value, key: &str) -> Option<usize> {
    props
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
}

fn prop_strings(props: &serde_json::Value, key: &str) -> Vec<String> {
    props
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn prop_rows(props: &serde_json::Value) -> Vec<Vec<String>> {
    props
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| {
                            cells
                                .iter()
                                .map(|cell| match cell {
                                    serde_json::Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn prop_pairs(props: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    props
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let label = item.get("label").and_then(serde_json::Value::as_str)?;
                    let value = item
                        .get("value")
                        .map(|value| match value {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    Some((label.to_owned(), value))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn size_from(props: &serde_json::Value) -> Size {
    Size::from_str(&prop_str(props, "size").unwrap_or_else(|| "md".into()))
}

fn tone_variant(tone: Option<&str>) -> TagVariant {
    match tone.unwrap_or("secondary") {
        "primary" => TagVariant::Primary,
        "success" => TagVariant::Success,
        "warning" => TagVariant::Warning,
        "danger" | "error" => TagVariant::Danger,
        "info" => TagVariant::Info,
        _ => TagVariant::Secondary,
    }
}

fn icon_from(name: &str) -> Icon {
    let icon = match name.to_ascii_lowercase().as_str() {
        "check" => IconName::Check,
        "warning" | "alert" => IconName::TriangleAlert,
        "error" | "x" | "close" => IconName::CircleX,
        "search" => IconName::Search,
        "settings" => IconName::Settings,
        "file" => IconName::File,
        "folder" => IconName::Folder,
        "terminal" => IconName::SquareTerminal,
        "code" => IconName::FileText,
        "play" => IconName::Play,
        "pause" => IconName::Pause,
        "plus" => IconName::Plus,
        "copy" => IconName::Copy,
        "external-link" => IconName::ExternalLink,
        "calendar" => IconName::Calendar,
        "bell" => IconName::Bell,
        "star" => IconName::Star,
        "heart" => IconName::Heart,
        "download" => IconName::ArrowDown,
        "upload" => IconName::ArrowUp,
        "zap" => IconName::BatteryCharging,
        "sparkles" => IconName::StarFill,
        "bot" => IconName::Bot,
        "chart" | "chart-pie" => IconName::ChartPie,
        "globe" => IconName::Globe,
        "github" => IconName::Github,
        "user" => IconName::CircleUser,
        "loader" => IconName::LoaderCircle,
        "clock" => IconName::Calendar,
        "trash" | "delete" => IconName::Delete,
        _ => IconName::Info,
    };
    Icon::new(icon)
}

fn render_node(
    ctx: &UiCtx,
    id: &str,
    depth: usize,
    host: &crate::UiHost,
    window: &mut Window,
    cx: &mut gpui::App,
) -> AnyElement {
    if depth > MAX_DEPTH {
        return error_chip(ctx.tokens, format!("Nesting deeper than {MAX_DEPTH}"));
    }
    let Some(node) = ctx.spec.elements.get(id) else {
        return error_chip(ctx.tokens, format!("Missing element `{id}`"));
    };
    if !prop_bool(&node.props, "visible").unwrap_or(true) {
        return div().into_any_element();
    }

    let mut kids: Vec<AnyElement> = Vec::with_capacity(node.children.len());
    for child in &node.children {
        kids.push(render_node(ctx, child, depth + 1, host, window, cx));
    }

    let element_id = SharedString::from(format!("{}-{}", ctx.id_prefix, id));
    let node_key = state_key(ctx, id);
    let props = &node.props;
    let tokens = ctx.tokens;

    match node.kind.to_ascii_lowercase().as_str() {
        "numberinput" | "number-input" => numeric::number_input(element_id, props, window, cx),
        "slider" => numeric::slider(element_id, props, window, cx),
        "select" => selection::select(element_id, props, window, cx),
        "input" | "passwordinput" => {
            let placeholder = prop_str(props, "placeholder").unwrap_or_default();
            let value = prop_str(props, "value").unwrap_or_default();
            let masked = node.kind.eq_ignore_ascii_case("passwordinput");
            let multiline = !masked && prop_bool(props, "multiline").unwrap_or(false);
            let input = window.use_keyed_state(element_id, cx, |window, cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
                    .masked(masked)
                    .multi_line(multiline)
            });
            let mut content = div().w_full().flex().flex_col().gap(px(4.0));
            if let Some(label) = prop_str(props, "label") {
                content = content.child(div().text_size(px(12.0)).child(label));
            }
            content
                .child(Input::new(&input).disabled(prop_bool(props, "disabled").unwrap_or(false)))
                .into_any_element()
        }
        "label" => div()
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tokens.text_primary)
            .child(prop_str(props, "text").unwrap_or_default())
            .into_any_element(),
        "field" => {
            let mut field = div().w_full().flex().flex_col().gap(px(4.0));
            if let Some(label) = prop_str(props, "label") {
                field = field.child(div().text_size(px(12.0)).child(label));
            }
            field = field.children(kids);
            if let Some(description) = prop_str(props, "description") {
                field = field.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(description),
                );
            }
            field.into_any_element()
        }
        "toolbar" => div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(
                prop_f64(props, "gap").unwrap_or(8.0).clamp(0.0, 64.0) as f32
            ))
            .children(kids)
            .into_any_element(),
        "meter" => {
            let min = prop_f64(props, "min").unwrap_or(0.0);
            let max = prop_f64(props, "max").unwrap_or(100.0);
            let value = prop_f64(props, "value").unwrap_or(min);
            let percent = if max > min {
                ((value - min) / (max - min) * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let mut meter = div().w_full().flex().flex_col().gap(px(4.0));
            if let Some(label) = prop_str(props, "label") {
                meter = meter.child(div().text_size(px(12.0)).child(label));
            }
            meter
                .child(Progress::new(element_id).value(percent as f32).w_full())
                .into_any_element()
        }
        "segmentedcontrol" | "segmented-control" => {
            let options = prop_strings(props, "options");
            let selected = state_index(ctx, &node_key, prop_usize(props, "selected").unwrap_or(0))
                .min(options.len().saturating_sub(1));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            let bar = TabBar::new(element_id)
                .children(options.into_iter().map(|label| Tab::new().label(label)))
                .selected_index(selected)
                .on_click(
                    host.listener(move |this: &mut AppState, value: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*value),
                            cx,
                        );
                    }),
                );
            let panel = kids.into_iter().nth(selected);
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(bar)
                .when_some(panel, |container, panel| container.child(panel))
                .into_any_element()
        }
        "carousel" => {
            let count = kids.len();
            let selected = state_index(ctx, &node_key, prop_usize(props, "selected").unwrap_or(0))
                .min(count.saturating_sub(1));
            let pane_id = ctx.pane_id;
            let previous_key = node_key.clone();
            let next_key = node_key.clone();
            let previous = Button::new(SharedString::from(format!("{element_id}-prev")))
                .label("Previous")
                .disabled(selected == 0 || count == 0)
                .on_click(host.listener(
                    move |this: &mut AppState, _: &gpui::ClickEvent, _, cx| {
                        this.set_ui_state(
                            pane_id,
                            previous_key.clone(),
                            UiStateValue::Index(selected.saturating_sub(1)),
                            cx,
                        );
                    },
                ));
            let next = Button::new(SharedString::from(format!("{element_id}-next")))
                .label("Next")
                .disabled(selected + 1 >= count)
                .on_click(host.listener(
                    move |this: &mut AppState, _: &gpui::ClickEvent, _, cx| {
                        this.set_ui_state(
                            pane_id,
                            next_key.clone(),
                            UiStateValue::Index(selected + 1),
                            cx,
                        );
                    },
                ));
            let panel = kids.into_iter().nth(selected);
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .when_some(panel, |container, panel| container.child(panel))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(previous)
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(tokens.text_muted)
                                .child(format!(
                                    "{} / {}",
                                    if count == 0 { 0 } else { selected + 1 },
                                    count
                                )),
                        )
                        .child(next),
                )
                .into_any_element()
        }
        "timeline" => {
            let mut timeline = div().w_full().flex().flex_col().gap(px(12.0));
            if let Some(items) = props.get("items").and_then(serde_json::Value::as_array) {
                for item in items.iter().take(MAX_NODES) {
                    let mut content = div().flex().flex_col().gap(px(3.0)).child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(prop_str(item, "title").unwrap_or_default()),
                    );
                    if let Some(description) = prop_str(item, "description") {
                        content = content.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(tokens.text_muted)
                                .child(description),
                        );
                    }
                    timeline = timeline.child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(div().text_color(tokens.text_muted).child("•"))
                            .child(content),
                    );
                }
            }
            timeline.into_any_element()
        }
        "card" | "group" | "section" => {
            let mut card = GroupBox::new().id(element_id.clone());
            if let Some(title) = prop_str(props, "title") {
                card = card.title(
                    div()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                );
            }
            match prop_str(props, "variant").as_deref() {
                Some("fill") => card = card.fill(),
                Some("outline") => card = card.outline(),
                _ => {}
            }
            card = card.children(kids);
            if let Some(footer) = prop_str(props, "footer") {
                card = card.footer(
                    div()
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(footer),
                );
            }
            card.into_any_element()
        }
        "row" | "hstack" => {
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(
                    prop_f64(props, "gap").unwrap_or(8.0).clamp(0.0, 64.0) as f32
                ));
            row = match prop_str(props, "align").as_deref() {
                Some("start") => row.items_start(),
                Some("end") => row.items_end(),
                Some("stretch") => row.items_stretch(),
                _ => row.items_center(),
            };
            row = match prop_str(props, "justify").as_deref() {
                Some("center") => row.justify_center(),
                Some("end") => row.justify_end(),
                Some("between") => row.justify_between(),
                _ => row.justify_start(),
            };
            if prop_bool(props, "wrap").unwrap_or(false) {
                row = row.flex_wrap();
            }
            row.children(kids).into_any_element()
        }
        "column" | "vstack" | "stack" => {
            let mut column =
                div().flex().flex_col().gap(px(prop_f64(props, "gap")
                    .unwrap_or(8.0)
                    .clamp(0.0, 64.0) as f32));
            column = match prop_str(props, "align").as_deref() {
                Some("center") => column.items_center(),
                Some("end") => column.items_end(),
                Some("stretch") => column.items_stretch(),
                _ => column.items_start(),
            };
            column.children(kids).into_any_element()
        }
        "grid" => {
            let cols = prop_usize(props, "cols").unwrap_or(2).clamp(1, 6);
            let gap = px(prop_f64(props, "gap").unwrap_or(8.0).clamp(0.0, 64.0) as f32);
            div()
                .w_full()
                .grid()
                .grid_cols(cols as u16)
                .gap(gap)
                .children(kids)
                .into_any_element()
        }
        "heading" => {
            let level = prop_usize(props, "level").unwrap_or(2).clamp(1, 4);
            let size = match level {
                1 => 20.0,
                2 => 17.0,
                3 => 15.0,
                _ => 13.5,
            };
            div()
                .w_full()
                .text_size(px(size))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(tokens.text_primary)
                .child(prop_str(props, "text").unwrap_or_default())
                .flex()
                .flex_col()
                .children(kids)
                .into_any_element()
        }
        "text" | "paragraph" => {
            let text = prop_str(props, "text").unwrap_or_default();
            if prop_bool(props, "markdown").unwrap_or(true) {
                let mut view = TextView::markdown(element_id, text);
                view = view.w_full();
                div()
                    .w_full()
                    .text_size(px(13.0))
                    .text_color(if prop_bool(props, "muted").unwrap_or(false) {
                        tokens.text_muted
                    } else {
                        tokens.text_primary
                    })
                    .child(view)
                    .children(kids)
                    .into_any_element()
            } else {
                div()
                    .w_full()
                    .whitespace_normal()
                    .text_size(px(13.0))
                    .text_color(if prop_bool(props, "muted").unwrap_or(false) {
                        tokens.text_muted
                    } else {
                        tokens.text_primary
                    })
                    .child(text)
                    .children(kids)
                    .into_any_element()
            }
        }
        "code" => {
            let mut block = div()
                .w_full()
                .min_w_0()
                .rounded(px(8.0))
                .border_1()
                .border_color(tokens.border_subtle)
                .bg(tokens.card_hover)
                .px(px(10.0))
                .py(px(8.0))
                .font_family(MONO_FONT)
                .text_size(px(11.5))
                .text_color(tokens.text_primary)
                .whitespace_normal();
            if let Some(lang) = prop_str(props, "lang") {
                block = block.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(tokens.text_muted)
                        .child(lang),
                );
            }
            block
                .child(prop_str(props, "text").unwrap_or_default())
                .children(kids)
                .into_any_element()
        }
        "divider" | "separator" => {
            let mut separator = Separator::horizontal();
            if let Some(label) = prop_str(props, "label") {
                separator = separator.label(label);
            }
            div()
                .w_full()
                .py(px(2.0))
                .child(separator)
                .into_any_element()
        }
        "tag" | "badge" => Tag::new()
            .with_variant(tone_variant(prop_str(props, "tone").as_deref()))
            .when(prop_bool(props, "outline").unwrap_or(false), |tag| {
                tag.outline()
            })
            .child(prop_str(props, "text").unwrap_or_default())
            .children(kids)
            .into_any_element(),
        "alert" => {
            let tone = prop_str(props, "tone").unwrap_or_else(|| "info".into());
            let mut alert = match tone.as_str() {
                "success" => {
                    Alert::success(element_id, prop_str(props, "message").unwrap_or_default())
                }
                "warning" => {
                    Alert::warning(element_id, prop_str(props, "message").unwrap_or_default())
                }
                "danger" | "error" => {
                    Alert::error(element_id, prop_str(props, "message").unwrap_or_default())
                }
                _ => Alert::info(element_id, prop_str(props, "message").unwrap_or_default()),
            };
            if let Some(title) = prop_str(props, "title") {
                alert = alert.title(title);
            }
            alert.into_any_element()
        }
        "progress" => {
            let value = prop_f64(props, "value").unwrap_or(0.0).clamp(0.0, 100.0) as f32;
            let loading = prop_bool(props, "loading").unwrap_or(false);
            let mut column = div().w_full().flex().flex_col().gap(px(4.0));
            if let Some(label) = prop_str(props, "label") {
                column = column.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(label),
                );
            }
            column
                .child(
                    Progress::new(element_id)
                        .value(value)
                        .loading(loading)
                        .w_full(),
                )
                .into_any_element()
        }
        "spinner" => div()
            .child(Spinner::new().with_size(size_from(props)))
            .into_any_element(),
        "skeleton" => {
            let mut skeleton = Skeleton::new().w_full();
            if let Some(width) = prop_f64(props, "width") {
                skeleton = skeleton.w(px(width as f32));
            }
            skeleton = skeleton.h(px(prop_f64(props, "height").unwrap_or(16.0) as f32));
            skeleton.into_any_element()
        }
        "avatar" => {
            let mut avatar = Avatar::new().with_size(size_from(props));
            if let Some(name) = prop_str(props, "name") {
                avatar = avatar.name(name);
            }
            if let Some(src) = prop_str(props, "src") {
                avatar = avatar.src(src);
            }
            avatar.into_any_element()
        }
        "icon" => {
            let icon = prop_str(props, "name").unwrap_or_else(|| "info".into());
            div()
                .text_color(tokens.text_secondary)
                .child(icon_from(&icon).with_size(size_from(props)))
                .into_any_element()
        }
        "kbd" => {
            let keys = prop_str(props, "keys").unwrap_or_default();
            match keystroke(&keys) {
                Some(stroke) => Kbd::new(stroke).into_any_element(),
                None => error_chip(tokens, format!("Invalid keys `{keys}`")),
            }
        }
        "descriptionlist" | "description-list" => {
            let mut list = DescriptionList::new()
                .columns(prop_usize(props, "columns").unwrap_or(1).clamp(1, 10))
                .bordered(prop_bool(props, "bordered").unwrap_or(false));
            for (label, value) in prop_pairs(props, "items") {
                list = list.child(DescriptionItem::new(label).value(value));
            }
            list.into_any_element()
        }
        "table" => {
            let columns = prop_strings(props, "columns");
            let rows = prop_rows(props);
            let mut table = Table::new().w_full();
            if !columns.is_empty() {
                table = table.child(
                    TableHeader::new().child(
                        TableRow::new().children(
                            columns
                                .iter()
                                .map(|column| TableHead::new().child(column.clone())),
                        ),
                    ),
                );
            }
            let body = rows.iter().fold(TableBody::new(), |body, row| {
                body.child(
                    TableRow::new()
                        .children(row.iter().map(|cell| TableCell::new().child(cell.clone()))),
                )
            });
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .when_some(prop_str(props, "caption"), |wrapper, caption| {
                    wrapper.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(tokens.text_muted)
                            .child(caption),
                    )
                })
                .child(table.child(body))
                .into_any_element()
        }
        "button" => {
            let label = prop_str(props, "label").unwrap_or_else(|| "Button".into());
            let mut button = Button::new(element_id)
                .label(label)
                .with_size(size_from(props));
            button = match prop_str(props, "tone").as_deref() {
                Some("primary") => button.primary(),
                Some("success") => button.success(),
                Some("warning") => button.warning(),
                Some("danger" | "error") => button.danger(),
                Some("info") => button.info(),
                Some("ghost") => button.ghost(),
                Some("text") => button.text(),
                _ => button.secondary(),
            };
            if prop_bool(props, "outline").unwrap_or(false) {
                button = button.outline();
            }
            if prop_bool(props, "disabled").unwrap_or(false) {
                button = button.disabled(true);
            }
            if let Some(action) = prop_str(props, "action") {
                let pane_id = ctx.pane_id;
                button = button.on_click(host.listener(
                    move |this: &mut AppState, _: &gpui::ClickEvent, window, cx| {
                        this.submit_ui_action(pane_id, action.clone(), window, cx);
                    },
                ));
            }
            button.into_any_element()
        }
        "checkbox" => {
            let checked = state_bool(ctx, &node_key, prop_bool(props, "checked").unwrap_or(false));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Checkbox::new(element_id)
                .label(prop_str(props, "label").unwrap_or_default())
                .checked(checked)
                .with_size(size_from(props))
                .on_change(
                    host.listener(move |this: &mut AppState, value: &bool, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Bool(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "switch" => {
            let checked = state_bool(ctx, &node_key, prop_bool(props, "checked").unwrap_or(false));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Switch::new(element_id)
                .label(prop_str(props, "label").unwrap_or_default())
                .checked(checked)
                .with_size(size_from(props))
                .on_change(
                    host.listener(move |this: &mut AppState, value: &bool, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Bool(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "radiogroup" | "radio-group" => {
            let selected = state_index(ctx, &node_key, prop_usize(props, "selected").unwrap_or(0));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            let mut group = RadioGroup::new(element_id)
                .children(prop_strings(props, "options"))
                .selected_index(Some(selected))
                .on_change(host.listener(
                    move |this: &mut AppState, value: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*value),
                            cx,
                        );
                    },
                ));
            if prop_bool(props, "horizontal").unwrap_or(false) {
                group = group.layout(gpui::Axis::Horizontal);
            }
            group.into_any_element()
        }
        "rating" => {
            let value = state_index(ctx, &node_key, prop_usize(props, "value").unwrap_or(0));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Rating::new(element_id)
                .value(value)
                .max(prop_usize(props, "max").unwrap_or(5))
                .on_click(
                    host.listener(move |this: &mut AppState, value: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "toggle" => {
            let checked = state_bool(ctx, &node_key, prop_bool(props, "checked").unwrap_or(false));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Toggle::new(element_id)
                .label(prop_str(props, "label").unwrap_or_default())
                .checked(checked)
                .with_size(size_from(props))
                .on_click(
                    host.listener(move |this: &mut AppState, value: &bool, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Bool(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "tabs" => {
            let selected = state_index(ctx, &node_key, prop_usize(props, "selected").unwrap_or(0));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            TabBar::new(element_id)
                .children(
                    prop_strings(props, "tabs")
                        .into_iter()
                        .map(|label| Tab::new().label(label)),
                )
                .selected_index(selected)
                .on_click(
                    host.listener(move |this: &mut AppState, value: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "breadcrumb" => Breadcrumb::new()
            .children(
                prop_strings(props, "items")
                    .into_iter()
                    .map(BreadcrumbItem::new),
            )
            .into_any_element(),
        "marker" => Marker::new()
            .content(MarkerContent::new().text(prop_str(props, "text").unwrap_or_default()))
            .into_any_element(),
        "stepper" => {
            let selected = state_index(ctx, &node_key, prop_usize(props, "selected").unwrap_or(0));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Stepper::new(element_id)
                .items(
                    prop_strings(props, "steps")
                        .into_iter()
                        .map(|step| StepperItem::new().child(step)),
                )
                .selected_index(selected)
                .on_click(
                    host.listener(move |this: &mut AppState, value: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*value),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "accordion" => {
            let items = props
                .get("items")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let default_open: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.get("open")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                })
                .map(|(ix, _)| ix)
                .collect();
            let open_indices = state_indices(ctx, &node_key, default_open);
            let multiple = prop_bool(props, "multiple").unwrap_or(false);
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            let mut accordion = Accordion::new(element_id)
                .multiple(multiple)
                .on_toggle_click(host.listener(
                    move |this: &mut AppState, indices: &[usize], _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Indices(indices.to_vec()),
                            cx,
                        );
                    },
                ));
            for (ix, item) in items.iter().enumerate() {
                let title = item
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Details")
                    .to_owned();
                let content = item
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let open = open_indices.contains(&ix);
                accordion = accordion.item(|entry| {
                    entry.title(title).open(open).child(
                        div()
                            .text_size(px(12.5))
                            .text_color(tokens.text_secondary)
                            .child(content),
                    )
                });
            }
            accordion.into_any_element()
        }
        "collapsible" => {
            let title = prop_str(props, "title").unwrap_or_else(|| "Details".into());
            let content = prop_str(props, "content").unwrap_or_default();
            let default_open = prop_bool(props, "open")
                .unwrap_or(false)
                .then_some(vec![0])
                .unwrap_or_default();
            let open = state_indices(ctx, &node_key, default_open).contains(&0);
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            Accordion::new(element_id)
                .on_toggle_click(host.listener(
                    move |this: &mut AppState, indices: &[usize], _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Indices(indices.to_vec()),
                            cx,
                        );
                    },
                ))
                .item(|entry| {
                    entry.title(title).open(open).child(
                        div()
                            .text_size(px(12.5))
                            .text_color(tokens.text_secondary)
                            .child(content),
                    )
                })
                .into_any_element()
        }
        "pagination" => {
            let page = state_index(ctx, &node_key, prop_usize(props, "page").unwrap_or(1));
            let pane_id = ctx.pane_id;
            let handler_key = node_key.clone();
            gpui_kit::component::pagination::Pagination::new(element_id)
                .current_page(page)
                .total_pages(prop_usize(props, "total").unwrap_or(1))
                .on_click(
                    host.listener(move |this: &mut AppState, page: &usize, _window, cx| {
                        this.set_ui_state(
                            pane_id,
                            handler_key.clone(),
                            UiStateValue::Index(*page),
                            cx,
                        );
                    }),
                )
                .into_any_element()
        }
        "chart" => render_chart(ctx, props, element_id),
        "metrics" => render_metrics(ctx, props),
        "heatmap" => render_heatmap(ctx, props),
        "stackedbar" => render_stacked_bar(ctx, props),
        "list" => {
            let ordered = prop_bool(props, "ordered").unwrap_or(false);
            let items = prop_strings(props, "items");
            let mut column = div().w_full().flex().flex_col().gap(px(4.0));
            for (ix, item) in items.into_iter().enumerate() {
                column = column.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .text_size(px(12.5))
                        .text_color(tokens.text_primary)
                        .child(div().text_color(tokens.text_muted).child(if ordered {
                            format!("{}.", ix + 1)
                        } else {
                            "•".to_string()
                        }))
                        .child(div().min_w_0().child(item)),
                );
            }
            column.into_any_element()
        }
        "diff" => div()
            .w_full()
            .min_w_0()
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.border_subtle)
            .bg(tokens.card_hover)
            .p(px(8.0))
            .font_family(MONO_FONT)
            .text_size(px(11.0))
            .whitespace_normal()
            .children(
                prop_str(props, "patch")
                    .unwrap_or_default()
                    .lines()
                    .map(|line| {
                        let color = if line.starts_with('+') {
                            tokens.block_success
                        } else if line.starts_with('-') {
                            tokens.block_failure
                        } else {
                            tokens.text_secondary
                        };
                        div().text_color(color).child(line.to_string())
                    }),
            )
            .into_any_element(),
        "link" => div()
            .text_color(tokens.text_primary)
            .underline()
            .child(prop_str(props, "text").unwrap_or_default())
            .into_any_element(),
        "bubble" | "message" => {
            let user = prop_str(props, "role").as_deref() == Some("user");
            div()
                .w_full()
                .flex()
                .when(user, |row| row.justify_end())
                .child(
                    div()
                        .max_w(relative(0.85))
                        .min_w_0()
                        .rounded(px(10.0))
                        .bg(if user {
                            tokens.card_hover
                        } else {
                            tokens.block_bg
                        })
                        .border_1()
                        .border_color(tokens.border_subtle)
                        .px(px(10.0))
                        .py(px(7.0))
                        .text_size(px(12.5))
                        .text_color(tokens.text_primary)
                        .child(prop_str(props, "text").unwrap_or_default())
                        .children(kids),
                )
                .into_any_element()
        }
        "divider-line" => div().into_any_element(),
        other => error_chip(tokens, format!("Unknown UI component `{other}`")),
    }
}

fn render_metrics(ctx: &UiCtx, props: &serde_json::Value) -> AnyElement {
    let Some(items) = props.get("items").and_then(|v| v.as_array()) else {
        return error_chip(ctx.tokens, "Metrics needs an `items` array".into());
    };
    let mut row = div().w_full().flex().flex_wrap().gap(px(16.0));
    for item in items.iter().take(12) {
        let value = prop_str(item, "value")
            .unwrap_or_else(|| item.get("value").map(|v| v.to_string()).unwrap_or_default());
        let label = prop_str(item, "label").unwrap_or_default();
        row = row.child(
            div()
                .min_w(px(95.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(ctx.tokens.text_primary)
                        .child(value),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(ctx.tokens.text_muted)
                        .child(label),
                ),
        );
    }
    row.into_any_element()
}

fn render_heatmap(ctx: &UiCtx, props: &serde_json::Value) -> AnyElement {
    let columns = prop_strings(props, "columns");
    let Some(rows) = props.get("rows").and_then(|v| v.as_array()) else {
        return error_chip(ctx.tokens, "Heatmap needs a `rows` array".into());
    };
    let values: Vec<f64> = rows
        .iter()
        .take(32)
        .flat_map(|row| {
            row.get("values")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .take(64)
                .filter_map(|v| v.as_f64())
        })
        .collect();
    if values.is_empty() {
        return error_chip(ctx.tokens, "Heatmap needs numeric values".into());
    }
    let min = prop_f64(props, "min")
        .unwrap_or_else(|| values.iter().copied().fold(f64::INFINITY, f64::min));
    let max = prop_f64(props, "max")
        .unwrap_or_else(|| values.iter().copied().fold(f64::NEG_INFINITY, f64::max));
    let palette = [
        0x112c52ff, 0x174782ff, 0x1e5da8ff, 0x347bdcff, 0x72a7eaff, 0xb8d5f8ff,
    ];
    let mut content = div().w_full().flex().flex_col().gap(px(2.0));
    for row in rows.iter().take(32) {
        let mut line = div().w_full().flex().items_center().gap(px(2.0)).child(
            div()
                .w(px(48.0))
                .flex_shrink_0()
                .text_size(px(10.0))
                .text_color(ctx.tokens.text_muted)
                .truncate()
                .child(prop_str(row, "label").unwrap_or_default()),
        );
        if let Some(cells) = row.get("values").and_then(|v| v.as_array()) {
            for cell in cells.iter().take(64) {
                let color = cell.as_f64().map(|value| {
                    let intensity = if max > min {
                        ((value - min) / (max - min)).clamp(0.0, 1.0)
                    } else {
                        0.5
                    };
                    gpui::rgba(palette[(intensity * 5.0).round() as usize])
                });
                line = line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(18.0))
                        .rounded(px(2.0))
                        .bg(color.map(Hsla::from).unwrap_or(ctx.tokens.border_subtle)),
                );
            }
        }
        content = content.child(line);
    }
    if !columns.is_empty() {
        let mut labels = div()
            .w_full()
            .flex()
            .gap(px(2.0))
            .child(div().w(px(48.0)).flex_shrink_0());
        for label in columns.into_iter().take(64) {
            labels = labels.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(9.0))
                    .text_color(ctx.tokens.text_muted)
                    .child(label),
            );
        }
        content = content.child(labels);
    }
    content.into_any_element()
}

fn render_stacked_bar(ctx: &UiCtx, props: &serde_json::Value) -> AnyElement {
    let colors = theme::visual_colors(ctx.tokens.is_dark);
    let series = prop_strings(props, "series");
    let Some(rows) = props.get("rows").and_then(|v| v.as_array()) else {
        return error_chip(ctx.tokens, "StackedBar needs a `rows` array".into());
    };
    let mut content = div().w_full().flex().flex_col().gap(px(8.0));
    let mut legend = div().flex().flex_wrap().gap(px(12.0));
    for (i, label) in series.iter().take(colors.len()).enumerate() {
        legend = legend.child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(10.0))
                .text_color(ctx.tokens.text_secondary)
                .child(
                    div()
                        .size(px(8.0))
                        .rounded(px(2.0))
                        .bg(gpui::rgba(colors[i])),
                )
                .child(label.clone()),
        );
    }
    content = content.child(legend);
    for row in rows.iter().take(32) {
        let values: Vec<f64> = row
            .get("values")
            .and_then(|v| v.as_array())
            .map(|v| {
                v.iter()
                    .take(colors.len())
                    .map(|x| x.as_f64().unwrap_or(0.0).max(0.0))
                    .collect()
            })
            .unwrap_or_default();
        let total: f64 = values.iter().sum();
        let mut bar = div().flex_1().min_w_0().h(px(24.0)).flex().gap(px(1.0));
        if total > 0.0 {
            for (i, value) in values.iter().enumerate() {
                if *value > 0.0 {
                    let fraction = (*value / total) as f32;
                    let mut segment = div()
                        .h_full()
                        .w(relative(fraction))
                        .rounded(px(2.0))
                        .bg(gpui::rgba(colors[i]))
                        .flex()
                        .items_center()
                        .justify_center();
                    if fraction >= 0.12 {
                        segment = segment.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(gpui::rgba(0xffffffff))
                                .child(format!("{:.0}%", fraction * 100.0)),
                        );
                    }
                    bar = bar.child(segment);
                }
            }
        }
        content = content.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .w(px(72.0))
                        .flex_shrink_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(ctx.tokens.text_primary)
                        .child(prop_str(row, "label").unwrap_or_default()),
                )
                .child(bar),
        );
    }
    content.into_any_element()
}

fn render_chart(ctx: &UiCtx, props: &serde_json::Value, id: SharedString) -> AnyElement {
    #[derive(Clone)]
    struct Point {
        label: String,
        value: f64,
    }

    let data: Vec<Point> = props
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| Point {
                    label: item
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    value: item
                        .get("value")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default();
    if data.is_empty() {
        return error_chip(ctx.tokens, "Chart needs a non-empty `data` array".into());
    }
    let height = prop_f64(props, "height")
        .unwrap_or(180.0)
        .clamp(60.0, 480.0) as f32;
    let kind = prop_str(props, "kind").unwrap_or_else(|| "bar".into());

    let chart: AnyElement = match kind.as_str() {
        "line" => LineChart::new(data)
            .id(id)
            .x(|point| point.label.clone())
            .y(|point| point.value)
            .into_any_element(),
        "pie" => PieChart::new(data)
            .id(id)
            .label(|point| point.label.clone().into())
            .value(|point| point.value as f32)
            .into_any_element(),
        _ => BarChart::new(data)
            .id(id)
            .band(|point| point.label.clone())
            .value(|point| point.value)
            .into_any_element(),
    };

    div()
        .w_full()
        .min_w_0()
        .h(px(height))
        .child(chart)
        .into_any_element()
}

fn keystroke(keys: &str) -> Option<gpui::Keystroke> {
    let os = if cfg!(target_os = "macos") {
        OperatingSystem::Mac
    } else if cfg!(target_os = "windows") {
        OperatingSystem::Windows
    } else {
        OperatingSystem::Linux
    };
    gpui::Keystroke::parse_with_os(keys, os).ok()
}

fn error_chip(tokens: ThemeTokens, message: String) -> AnyElement {
    div()
        .rounded(px(6.0))
        .border_1()
        .border_color(tokens.block_failure.opacity(0.4))
        .bg(tokens.block_failure.opacity(0.08))
        .px(px(8.0))
        .py(px(4.0))
        .text_size(px(11.0))
        .text_color(tokens.block_failure)
        .child(message)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_markdown_and_ui_fences() {
        let text = "Before\n```ui\n{\"a\":1}\n```\nAfter";
        let segments = split_ui_segments(text);
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], Segment::Markdown(text) if text.contains("Before")));
        assert!(
            matches!(segments[1], Segment::Ui { raw, closed: true } if raw.trim() == "{\"a\":1}")
        );
        assert!(matches!(segments[2], Segment::Markdown(text) if text.contains("After")));
    }

    #[test]
    fn ui_fences_inside_code_examples_remain_markdown() {
        let text = "~~~~markdown\n```ui\n{\"root\":\"example\"}\n```\n~~~~\n\n```ui\n{}\n```";
        let segments = split_ui_segments(text);
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[0], Segment::Markdown(code) if code.contains("```ui")));
        assert!(matches!(segments[1], Segment::Ui { raw, closed: true } if raw.trim() == "{}"));
    }

    #[test]
    fn indented_fence_does_not_close_ui_block() {
        let segments = split_ui_segments("```ui\n    ```\n{}\n```\n");
        assert!(
            matches!(segments.as_slice(), [Segment::Ui { raw, closed: true }] if raw.contains("    ```\n{}"))
        );
    }

    #[test]
    fn keeps_an_open_fence_pending() {
        let segments = split_ui_segments("Hi\n```ui\n{\"root\":\"x\"");
        assert!(matches!(segments[0], Segment::Markdown(text) if text.contains("Hi")));
        assert!(matches!(segments[1], Segment::Ui { closed: false, .. }));
    }

    #[test]
    fn renders_complete_spec_before_closing_fence() {
        let mut body = AssistantBody::default();
        body.prepare("```ui\n{\"root\":\"a\",\"elements\":{\"a\":{\"type\":\"Text\"}}}");
        assert!(matches!(body.segments[0], Some(PreparedSegment::Ui(Ok(_)))));

        body.prepare("```ui\n{\"root\":\"a\"");
        assert!(matches!(body.segments[0], Some(PreparedSegment::PendingUi)));
    }

    #[test]
    fn previews_complete_nodes_while_later_nodes_stream() {
        let raw = r#"{"root":"a","elements":{"a":{"type":"Card","children":["b"]},"b":{"type":"Text","props":{"text":"hello } world"}},"c":{"type":"Text"#;
        let spec = streaming::partial_spec(raw).expect("complete nodes can be previewed");
        assert_eq!(spec.elements.len(), 2);
        assert_eq!(spec.elements["a"].children, ["b"]);

        let parent_only =
            r#"{"root":"a","elements":{"a":{"type":"Card","children":["b"]},"b":{"type":"Text"#;
        let spec = streaming::partial_spec(parent_only).expect("parent can be previewed");
        assert!(spec.elements["a"].children.is_empty());
    }

    #[test]
    fn keeps_preview_during_an_unfinished_next_node() {
        let mut body = AssistantBody::default();
        let first = "```ui\n{\"root\":\"a\",\"elements\":{\"a\":{\"type\":\"Card\"}";
        body.prepare(first);
        assert!(matches!(body.segments[0], Some(PreparedSegment::Ui(Ok(_)))));
        body.prepare(&format!("{first},\"b\":{{\"type\":\"Text\""));
        assert!(matches!(body.segments[0], Some(PreparedSegment::Ui(Ok(_)))));
        body.prepare("```ui\n{\"root\":");
        assert!(matches!(body.segments[0], Some(PreparedSegment::PendingUi)));
    }

    #[test]
    fn rejects_exponentially_expanding_element_graph() {
        let mut elements = serde_json::Map::new();
        for index in 0..10 {
            let children = if index == 9 {
                vec![]
            } else {
                vec![format!("n{}", index + 1); 2]
            };
            elements.insert(
                format!("n{index}"),
                serde_json::json!({"type":"Column", "children": children}),
            );
        }
        let raw = serde_json::json!({"root":"n0", "elements":elements}).to_string();
        assert!(parse_spec(&raw).unwrap_err().contains("expands"));
    }

    #[test]
    fn ignores_other_fences() {
        let text = "```json\n{\"a\":1}\n```";
        let segments = split_ui_segments(text);
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Markdown(_)));
    }

    #[test]
    fn parses_a_valid_spec() {
        let spec = parse_spec(
            r#"{"root":"a","elements":{"a":{"type":"Card","children":["b"]},"b":{"type":"Text","props":{"text":"hi"}}}}"#,
        )
        .expect("valid");
        assert_eq!(spec.root, "a");
        assert_eq!(spec.elements.len(), 2);
    }

    #[test]
    fn catalog_example_parses_as_a_ui_spec() {
        let segments = split_ui_segments(CATALOG_PROMPT);
        let [
            Segment::Markdown(_),
            Segment::Ui { raw, closed: true },
            Segment::Markdown(_),
        ] = segments.as_slice()
        else {
            panic!("catalog should contain one closed UI fence");
        };
        let spec = parse_spec(raw).expect("catalog example must be a valid UI spec");
        assert_eq!(spec.elements[&spec.root].kind, "Card");
        assert_eq!(spec.elements[&spec.root].children.len(), 4);
    }

    #[test]
    fn rejects_missing_root_and_cycles() {
        assert!(parse_spec(r#"{"root":"nope","elements":{}}"#).is_err());
        assert!(
            parse_spec(r#"{"root":"a","elements":{"a":{"type":"Card","children":["a"]}}}"#)
                .is_err()
        );
    }
}
