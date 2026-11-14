use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Element, ElementId, Entity,
    EntityId, FocusHandle, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement,
    KeyBinding, LayoutId, ListState, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point, RenderOnce, SharedString, Size, StyleRefinement, Styled, Window, div, px,
};

use crate::highlighter::HighlightTheme;
use crate::scroll::ScrollableElement;
use crate::text::node::CodeBlock;
use crate::{ActiveTheme, StyledExt, v_flex};
use crate::{
    global_state::GlobalState,
    input::{self},
    text::{
        TextViewStyle,
        node::{self, NodeContext},
    },
};

const CONTEXT: &'static str = "TextView";

// Opt-in parse timings for the streaming chat pipeline. The app enables these
// with `KATHAMO_STREAM_TIMINGS=1`; with the variable unset no samples are
// retained. Only durations are recorded.
static TIMING_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0")
});
static PARSE_SAMPLES: LazyLock<Mutex<Vec<(&'static str, u64)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static PARSE_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static PARSE_NEXT_REPORT_MS: AtomicU64 = AtomicU64::new(0);

fn record_parse(label: &'static str, start: Instant) {
    if !*TIMING_ENABLED {
        return;
    }
    let micros = start.elapsed().as_micros().min(u128::from(u32::MAX)) as u64;
    {
        let mut samples = PARSE_SAMPLES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Bound opt-in instrumentation as well as the document pipeline.
        if samples.len() < 4096 {
            samples.push((label, micros));
        }
    }
    maybe_report_parses();
}

fn maybe_report_parses() {
    let elapsed = PARSE_ORIGIN.elapsed().as_millis() as u64;
    let next = PARSE_NEXT_REPORT_MS.load(Ordering::Relaxed);
    if next == 0 {
        PARSE_NEXT_REPORT_MS.store(elapsed + 5_000, Ordering::Relaxed);
        return;
    }
    if elapsed < next {
        return;
    }
    PARSE_NEXT_REPORT_MS.store(elapsed + 5_000, Ordering::Relaxed);
    let samples = {
        let mut samples = PARSE_SAMPLES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *samples)
    };
    if samples.is_empty() {
        return;
    }
    eprintln!("kathamo markdown parse timings (us, per report window):");
    for label in [
        "markdown.initial_parse",
        "markdown.update_parse",
        "markdown.queue_wait",
        "markdown.result_wait",
        "markdown.superseded_parse",
    ] {
        let mut values: Vec<u64> = samples
            .iter()
            .filter(|(sample, _)| *sample == label)
            .map(|(_, value)| *value)
            .collect();
        if values.is_empty() {
            continue;
        }
        values.sort_unstable();
        let percentile = |fraction: f64| {
            let index = ((values.len() as f64 * fraction) as usize).min(values.len() - 1);
            values[index]
        };
        eprintln!(
            "  {label:<24} n={:<6} p50={:<8} p95={:<8} max={}",
            values.len(),
            percentile(0.50),
            percentile(0.95),
            values.last().copied().unwrap_or(0),
        );
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", input::Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", input::Copy, Some(CONTEXT)),
    ]);
}

#[derive(IntoElement, Clone)]
struct TextViewElement {
    list_state: Option<ListState>,
    state: Entity<TextViewState>,
}

impl RenderOnce for TextViewElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, cx| {
            v_flex()
                .size_full()
                .map(|this| match &mut state.parsed_result {
                    Some(Ok(content)) => this.child(content.root_node.render_root(
                        self.list_state.clone(),
                        &content.node_cx,
                        window,
                        cx,
                    )),
                    Some(Err(err)) => this.child(
                        v_flex()
                            .gap_1()
                            .child("Failed to parse content")
                            .child(err.to_string()),
                    ),
                    None => this,
                })
        })
    }
}

/// Type for code block actions generator function.
pub(crate) type CodeBlockActionsFn =
    dyn Fn(&CodeBlock, &mut Window, &mut App) -> AnyElement + Send + Sync;

/// A text view that can render Markdown or HTML.
///
/// ## Goals
///
/// - Provide a rich text rendering component for such as Markdown or HTML,
/// used to display rich text in GPUI application (e.g., Help messages, Release notes)
/// - Support Markdown GFM and HTML (Simple HTML like Safari Reader Mode) for showing most common used markups.
/// - Support Heading, Paragraph, Bold, Italic, StrikeThrough, Code, Link, Image, Blockquote, List, Table, HorizontalRule, CodeBlock ...
///
/// ## Not Goals
///
/// - Customization of the complex style (some simple styles will be supported)
/// - As a Markdown editor or viewer (If you want to like this, you must fork your version).
/// - As a HTML viewer, we not support CSS, we only support basic HTML tags for used to as a content reader.
///
/// See also [`MarkdownElement`], [`HtmlElement`]
#[derive(Clone)]
pub struct TextView {
    id: ElementId,
    init_state: Option<InitState>,
    raw: SharedString,
    state: Entity<TextViewState>,
    style: StyleRefinement,
    selectable: bool,
    scrollable: bool,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
}

#[derive(PartialEq)]
pub(crate) struct ParsedContent {
    pub(crate) root_node: node::Node,
    pub(crate) node_cx: node::NodeContext,
}

/// The type of the text view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextViewType {
    /// Markdown view
    Markdown,
    /// HTML view
    Html,
}

enum Update {
    Text(SharedString),
    Style(TextViewStyle),
}

#[derive(Clone)]
struct ParseRequest {
    type_: TextViewType,
    text: SharedString,
    style: TextViewStyle,
    highlight_theme: Arc<HighlightTheme>,
}

struct ParseResult {
    generation: u64,
    epoch: u64,
    content: Result<ParsedContent, SharedString>,
    completed_at: Instant,
}

struct ParseMailbox {
    request: ParseRequest,
    generation: u64,
    // Appends share an epoch: a completed prefix may make progress while the
    // next prefix parses. Replacements/context changes invalidate all old work.
    epoch: u64,
    last_applied: Option<u64>,
    submitted_at: Instant,
    result: Option<ParseResult>,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
}

/// Both channels carry wakeups only. The mailbox retains one desired document
/// and one completed result, irrespective of producer or foreground speed.
#[derive(Clone)]
struct UpdateHandle {
    mailbox: Arc<Mutex<ParseMailbox>>,
    wake: smol::channel::Sender<()>,
}

impl UpdateHandle {
    fn send(&self, update: Update) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        let changed = match update {
            Update::Text(text) if mailbox.request.text != text => {
                if !text.starts_with(mailbox.request.text.as_str()) {
                    mailbox.epoch += 1;
                }
                mailbox.request.text = text;
                true
            }
            Update::Style(style) => {
                let changed = !same_style(&mailbox.request.style, &style);
                if changed {
                    mailbox.epoch += 1;
                }
                // Heading callbacks are evaluated only during rendering. Their
                // identity changes every frame for ordinary builder closures.
                mailbox.request.style = style;
                changed
            }
            _ => false,
        };
        if changed {
            mailbox.generation += 1;
            mailbox.submitted_at = Instant::now();
            // Full means a wakeup is already pending, not a dropped update.
            let _ = self.wake.try_send(());
        }
    }

    fn update_context(&self, type_: TextViewType, highlight_theme: &Arc<HighlightTheme>) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        if mailbox.request.type_ != type_
            || !Arc::ptr_eq(&mailbox.request.highlight_theme, highlight_theme)
        {
            mailbox.request.type_ = type_;
            mailbox.request.highlight_theme = highlight_theme.clone();
            mailbox.generation += 1;
            mailbox.epoch += 1;
            mailbox.submitted_at = Instant::now();
            let _ = self.wake.try_send(());
        }
    }

    fn take_result(mailbox: &Mutex<ParseMailbox>) -> Option<ParsedContentResult> {
        let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
        let mut result = mailbox.result.take()?;
        if result.epoch != mailbox.epoch
            || mailbox.last_applied.is_some_and(|generation| generation >= result.generation)
        {
            drop(mailbox);
            return None;
        }
        mailbox.last_applied = Some(result.generation);
        record_parse("markdown.result_wait", result.completed_at);
        if let Ok(content) = &mut result.content {
            content.node_cx.code_block_actions = mailbox.code_block_actions.clone();
            content.node_cx.style.heading_font_size =
                mailbox.request.style.heading_font_size.clone();
        }
        drop(mailbox);
        Some(result.content)
    }
}

type ParsedContentResult = Result<ParsedContent, SharedString>;

fn same_style(left: &TextViewStyle, right: &TextViewStyle) -> bool {
    // The heading callback is refreshed directly on existing/native content;
    // callback identity must never schedule another parse/render cycle.
    left == right && left.code_block == right.code_block && left.is_dark == right.is_dark
}

async fn parse_updates(
    mailbox: Arc<Mutex<ParseMailbox>>,
    wake: smol::channel::Receiver<()>,
    result_wake: smol::channel::Sender<()>,
) {
    let mut last_generation = None;
    while wake.recv().await.is_ok() {
        // Capacity is one. Consume its token before snapshotting, so an update
        // arriving during a parse always schedules another pass. The mailbox
        // coalesces bursts without delaying an update that is ready to parse.
        let _ = wake.try_recv();
        let (generation, epoch, request, submitted_at) = {
            let mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
            if last_generation == Some(mailbox.generation) {
                continue;
            }
            (mailbox.generation, mailbox.epoch, mailbox.request.clone(), mailbox.submitted_at)
        };
        record_parse("markdown.queue_wait", submitted_at);
        let parse_start = Instant::now();
        let content = parse_content(
            request.type_,
            &request.text,
            request.style,
            &request.highlight_theme,
            &None,
        );
        record_parse(
            if last_generation.is_none() {
                "markdown.initial_parse"
            } else {
                "markdown.update_parse"
            },
            parse_start,
        );
        last_generation = Some(generation);
        let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
        if mailbox.epoch != epoch {
            record_parse("markdown.superseded_parse", parse_start);
            continue;
        }
        let previous = mailbox.result.replace(ParseResult {
            generation,
            epoch,
            content,
            completed_at: Instant::now(),
        });
        let _ = result_wake.try_send(());
        // Releasing a full parsed tree must not hold up UI submissions.
        drop(mailbox);
        drop(previous);
    }
}

#[derive(Clone)]
enum InitState {
    Initializing {
        type_: TextViewType,
        text: SharedString,
        style: Box<TextViewStyle>,
        highlight_theme: Arc<HighlightTheme>,
    },
    Initialized {
        tx: UpdateHandle,
    },
}

pub(crate) struct TextViewState {
    parent_entity: Option<EntityId>,
    tx: Option<UpdateHandle>,
    parsed_result: Option<Result<ParsedContent, SharedString>>,
    focus_handle: Option<FocusHandle>,

    bounds: Bounds<Pixels>,

    selection_positions: (Option<Point<Pixels>>, Option<Point<Pixels>>),

    is_selecting: bool,
    is_selectable: bool,
    list_state: ListState,
}

impl TextViewState {
    fn new(cx: &mut Context<TextViewState>) -> Self {
        let focus_handle = cx.focus_handle();
        Self {
            parent_entity: None,
            tx: None,
            parsed_result: None,
            focus_handle: Some(focus_handle),
            bounds: Bounds::default(),
            selection_positions: (None, None),
            is_selecting: false,
            is_selectable: false,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
        }
    }
}

impl TextViewState {

    fn update_bounds(&mut self, bounds: Bounds<Pixels>) {
        if self.bounds.size != bounds.size {
            self.clear_selection();
        }
        self.bounds = bounds;
    }

    fn clear_selection(&mut self) {
        self.selection_positions = (None, None);
        self.is_selecting = false;
    }

    fn start_selection(&mut self, pos: Point<Pixels>) {
        let pos = pos - self.bounds.origin;
        self.selection_positions = (Some(pos), Some(pos));
        self.is_selecting = true;
    }

    fn update_selection(&mut self, pos: Point<Pixels>) {
        let pos = pos - self.bounds.origin;
        if let (Some(start), Some(_)) = self.selection_positions {
            self.selection_positions = (Some(start), Some(pos))
        }
    }

    fn end_selection(&mut self) {
        self.is_selecting = false;
    }

    pub(crate) fn has_selection(&self) -> bool {
        if let (Some(start), Some(end)) = self.selection_positions {
            start != end
        } else {
            false
        }
    }

    pub(crate) fn is_selectable(&self) -> bool {
        self.is_selectable
    }

    pub(crate) fn selection_bounds(&self) -> Bounds<Pixels> {
        selection_bounds(
            self.selection_positions.0,
            self.selection_positions.1,
            self.bounds,
        )
    }

    fn selection_text(&self) -> Option<String> {
        Some(
            self.parsed_result
                .as_ref()?
                .as_ref()
                .ok()?
                .root_node
                .selected_text(),
        )
    }
}

#[derive(IntoElement, Clone)]
pub enum Text {
    String(SharedString),
    TextView(Box<TextView>),
}

impl From<SharedString> for Text {
    fn from(s: SharedString) -> Self {
        Self::String(s)
    }
}

impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Self::String(SharedString::from(s.to_string()))
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Self::String(s.into())
    }
}

impl From<TextView> for Text {
    fn from(e: TextView) -> Self {
        Self::TextView(Box::new(e))
    }
}

impl Text {

    pub fn style(self, style: TextViewStyle) -> Self {
        match self {
            Self::String(s) => Self::String(s),
            Self::TextView(e) => Self::TextView(Box::new(e.style(style))),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::String(s) => s.as_str(),
            Self::TextView(view) => view.raw.as_str(),
        }
    }
}

impl RenderOnce for Text {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        match self {
            Self::String(s) => s.into_any_element(),
            Self::TextView(e) => e.into_any_element(),
        }
    }
}

impl Styled for TextView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl TextView {
    fn create_init_state(
        type_: TextViewType,
        text: &SharedString,
        highlight_theme: &Arc<HighlightTheme>,
        state: &Entity<TextViewState>,
        cx: &mut App,
    ) -> InitState {
        let state = state.read(cx);
        if let Some(tx) = &state.tx {
            tx.update_context(type_, highlight_theme);
            InitState::Initialized { tx: tx.clone() }
        } else {
            InitState::Initializing {
                type_,
                text: text.clone(),
                style: Default::default(),
                highlight_theme: highlight_theme.clone(),
            }
        }
    }

    pub fn markdown(
        id: impl Into<ElementId>,
        markdown: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let id: ElementId = id.into();
        let markdown = markdown.into();
        let highlight_theme = cx.theme().highlight_theme.clone();
        let state =
            window.use_keyed_state(SharedString::from(format!("{}/state", id)), cx, |_, cx| {
                TextViewState::new(cx)
            });
        let init_state = Self::create_init_state(
            TextViewType::Markdown,
            &markdown,
            &highlight_theme,
            &state,
            cx,
        );
        if let Some(tx) = &state.read(cx).tx {
            tx.send(Update::Text(markdown.clone()));
        }
        Self {
            id,
            init_state: Some(init_state),
            raw: markdown.clone(),
            style: StyleRefinement::default(),
            state,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
        }
    }

    pub fn html(
        id: impl Into<ElementId>,
        html: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let id: ElementId = id.into();
        let html = html.into();
        let highlight_theme = cx.theme().highlight_theme.clone();
        let state =
            window.use_keyed_state(SharedString::from(format!("{}/state", id)), cx, |_, cx| {
                TextViewState::new(cx)
            });
        let init_state =
            Self::create_init_state(TextViewType::Html, &html, &highlight_theme, &state, cx);
        if let Some(tx) = &state.read(cx).tx {
            tx.send(Update::Text(html.clone()));
        }
        Self {
            id,
            init_state: Some(init_state),
            style: StyleRefinement::default(),
            state,
            raw: html,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
        }
    }

    pub fn text(mut self, raw: impl Into<SharedString>) -> Self {
        let raw: SharedString = raw.into();
        if let Some(init_state) = &mut self.init_state {
            match init_state {
                InitState::Initializing { text, .. } => *text = raw.clone(),
                InitState::Initialized { tx } => {
                    tx.send(Update::Text(raw.clone()));
                }
            }
        }
        self.raw = raw;
        self
    }

    pub fn style(mut self, style: TextViewStyle) -> Self {
        if let Some(init_state) = &mut self.init_state {
            match init_state {
                InitState::Initializing { style: s, .. } => **s = style,
                InitState::Initialized { tx } => {
                    tx.send(Update::Style(style));
                }
            }
        }
        self
    }

    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    fn on_action_copy(state: &Entity<TextViewState>, cx: &mut App) {
        let Some(selected_text) = state.read(cx).selection_text() else {
            return;
        };

        cx.write_to_clipboard(ClipboardItem::new_string(selected_text.trim().to_string()));
    }

    pub fn code_block_actions<F, E>(mut self, f: F) -> Self
    where
        F: Fn(&CodeBlock, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        self.code_block_actions = Some(Arc::new(move |code_block, window, cx| {
            f(&code_block, window, cx).into_any_element()
        }));
        self
    }
}

impl IntoElement for TextView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextView {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some(InitState::Initializing {
            type_,
            text,
            style,
            highlight_theme,
        }) = self.init_state.take()
        {
            let style = *style;
            let (wake, rx) = smol::channel::bounded(1);
            let (result_wake, rx_result) = smol::channel::bounded(1);
            let mailbox = Arc::new(Mutex::new(ParseMailbox {
                request: ParseRequest {
                    type_,
                    text: text.clone(),
                    style: style.clone(),
                    highlight_theme,
                },
                generation: 0,
                epoch: 0,
                last_applied: None,
                submitted_at: Instant::now(),
                result: None,
                code_block_actions: self.code_block_actions.clone(),
            }));
            let tx = UpdateHandle { mailbox: mailbox.clone(), wake };
            self.state.update(cx, {
                let tx = tx.clone();
                |state, _| {
                    // Native inline text is immediately visible and selectable,
                    // without running Markdown or syntax highlighting on GPUI.
                    // HTML waits for its parser rather than flashing raw tags.
                    state.parsed_result = (type_ == TextViewType::Markdown).then(|| {
                        Ok(plain_content(text, style, self.code_block_actions.clone()))
                    });
                    state.tx = Some(tx);
                }
            });

            cx.spawn({
                let state = self.state.downgrade();
                // Do not retain an update sender here: dropping the keyed state
                // must close the worker input and then this result receiver.
                let mailbox = mailbox.clone();
                async move |cx| {
                    while rx_result.recv().await.is_ok() {
                        let Some(state) = state.upgrade() else { break };
                        _ = state.update(cx, |state, cx| {
                            // Recheck context epoch and monotonic progress: the
                            // foreground may have stalled since completion.
                            let Some(parsed_result) = UpdateHandle::take_result(&mailbox) else { return };
                            state.parsed_result = Some(parsed_result);
                            if let Some(parent_entity) = state.parent_entity {
                                let app = &mut **cx;
                                app.notify(parent_entity);
                            }
                            state.clear_selection();
                        });
                    }
                }
            })
            .detach();

            let _ = tx.wake.try_send(());
            cx.background_spawn(parse_updates(mailbox, rx, result_wake)).detach();
            self.init_state = Some(InitState::Initialized { tx });
        }

        self.state.update(cx, |state, _| {
            if let Some(tx) = &state.tx {
                let mut mailbox = tx.mailbox.lock().unwrap_or_else(|error| error.into_inner());
                mailbox.code_block_actions = self.code_block_actions.clone();
                if let Some(Ok(content)) = &mut state.parsed_result {
                    content.node_cx.code_block_actions = self.code_block_actions.clone();
                    content.node_cx.style.heading_font_size =
                        mailbox.request.style.heading_font_size.clone();
                }
            }
        });

        let list_state = &self.state.read(cx).list_state;

        let focus_handle = self
            .state
            .read(cx)
            .focus_handle
            .as_ref()
            .expect("focus_handle should init by TextViewState::new");

        let mut el = div()
            .key_context(CONTEXT)
            .track_focus(focus_handle)
            .size_full()
            .relative()
            .on_action({
                let state = self.state.clone();
                move |_: &input::Copy, _, cx| {
                    Self::on_action_copy(&state, cx);
                }
            })
            .child(TextViewElement {
                list_state: if self.scrollable {
                    Some(list_state.clone())
                } else {
                    None
                },
                state: self.state.clone(),
            })
            .refine_style(&self.style)
            .vertical_scrollbar(list_state)
            .into_any_element();
        let layout_id = el.request_layout(window, cx);
        (layout_id, el)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        request_layout.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let entity_id = window.current_view();
        let is_selectable = self.selectable;

        self.state.update(cx, |state, _| {
            state.parent_entity = Some(entity_id);
            state.update_bounds(bounds);
            state.is_selectable = is_selectable;
        });

        GlobalState::global_mut(cx)
            .text_view_state_stack
            .push(self.state.clone());
        request_layout.paint(window, cx);
        GlobalState::global_mut(cx).text_view_state_stack.pop();

        if self.selectable {
            let is_selecting = self.state.read(cx).is_selecting;
            let has_selection = self.state.read(cx).has_selection();

            window.on_mouse_event({
                let state = self.state.clone();
                move |event: &MouseDownEvent, phase, _, cx| {
                    if !bounds.contains(&event.position) || !phase.bubble() {
                        return;
                    }

                    state.update(cx, |state, _| {
                        state.start_selection(event.position);
                    });
                    cx.notify(entity_id);
                }
            });

            if is_selecting {
                // move to update end position.
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |event: &MouseMoveEvent, phase, _, cx| {
                        if !phase.bubble() {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.update_selection(event.position);
                        });
                        cx.notify(entity_id);
                    }
                });

                // up to end selection
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |_: &MouseUpEvent, phase, _, cx| {
                        if !phase.bubble() {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.end_selection();
                        });
                        cx.notify(entity_id);
                    }
                });
            }

            if has_selection {
                // down outside to clear selection
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |event: &MouseDownEvent, _, _, cx| {
                        if bounds.contains(&event.position) {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.clear_selection();
                        });
                        cx.notify(entity_id);
                    }
                });
            }
        }
    }
}

/// Constant-size native-node construction; the source string is shared, not
/// copied, and existing inline selection/measurement caches remain in use.
fn plain_content(
    text: SharedString,
    style: TextViewStyle,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
) -> ParsedContent {
    ParsedContent {
        root_node: node::Node::Root {
            children: vec![node::Node::Paragraph(node::Paragraph {
                children: vec![node::InlineNode::new(text)],
                ..Default::default()
            })],
        },
        node_cx: NodeContext {
            style,
            code_block_actions,
            ..Default::default()
        },
    }
}

fn parse_content(
    type_: TextViewType,
    text: &str,
    style: TextViewStyle,
    highlight_theme: &HighlightTheme,
    code_block_actions: &Option<Arc<CodeBlockActionsFn>>,
) -> Result<ParsedContent, SharedString> {
    let mut node_cx = NodeContext {
        style: style.clone(),
        code_block_actions: code_block_actions.clone(),
        ..NodeContext::default()
    };

    let res = match type_ {
        TextViewType::Markdown => {
            super::format::markdown::parse(text, &style, &mut node_cx, highlight_theme)
        }
        TextViewType::Html => super::format::html::parse(text, &mut node_cx),
    };
    res.map(move |root_node| ParsedContent { root_node, node_cx })
}

fn selection_bounds(
    start: Option<Point<Pixels>>,
    end: Option<Point<Pixels>>,
    bounds: Bounds<Pixels>,
) -> Bounds<Pixels> {
    if let (Some(start), Some(end)) = (start, end) {
        let start = start + bounds.origin;
        let end = end + bounds.origin;

        let origin = Point {
            x: start.x.min(end.x),
            y: start.y.min(end.y),
        };
        let size = Size {
            width: (start.x - end.x).abs(),
            height: (start.y - end.y).abs(),
        };

        return Bounds { origin, size };
    }

    Bounds::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, point, px, size};

    fn pipeline(
        text: &str,
    ) -> (
        UpdateHandle,
        smol::channel::Receiver<()>,
        smol::channel::Sender<()>,
        smol::channel::Receiver<()>,
    ) {
        let (wake, rx) = smol::channel::bounded(1);
        let (result_wake, results) = smol::channel::bounded(1);
        let handle = UpdateHandle {
            mailbox: Arc::new(Mutex::new(ParseMailbox {
                request: ParseRequest {
                    type_: TextViewType::Markdown,
                    text: text.to_owned().into(),
                    style: TextViewStyle::default(),
                    highlight_theme: HighlightTheme::default_light().clone(),
                },
                generation: 0,
                epoch: 0,
                last_applied: None,
                submitted_at: Instant::now(),
                result: None,
                code_block_actions: None,
            })),
            wake,
        };
        (handle, rx, result_wake, results)
    }

    #[test]
    fn test_streaming_mailbox_latest_text_and_style_are_bounded() {
        let (handle, wake, _, _) = pipeline("initial");
        for index in 0..10_000 {
            handle.send(Update::Text(format!("text {index}").into()));
        }
        let mut style = TextViewStyle::default();
        style.is_dark = true;
        style.code_block = StyleRefinement::default().bg(gpui::red());
        handle.send(Update::Style(style.clone()));
        handle.send(Update::Text("final".into()));
        assert_eq!(wake.len(), 1);
        let generation = {
            let mailbox = handle.mailbox.lock().unwrap();
            assert_eq!(mailbox.request.text.as_str(), "final");
            assert!(same_style(&mailbox.request.style, &style));
            assert!(mailbox.result.is_none());
            mailbox.generation
        };
        handle.send(Update::Text("final".into()));
        handle.send(Update::Style(style));
        assert_eq!(handle.mailbox.lock().unwrap().generation, generation);
        assert_eq!(wake.len(), 1);
    }

    #[test]
    fn test_streaming_mailbox_rejects_stale_results_and_uses_current_actions() {
        let (handle, _, _, _) = pipeline("old");
        handle.mailbox.lock().unwrap().result = Some(ParseResult {
            generation: 0,
            epoch: 0,
            content: Ok(plain_content("old".into(), TextViewStyle::default(), None)),
            completed_at: Instant::now(),
        });
        handle.send(Update::Text("new".into()));
        assert!(UpdateHandle::take_result(&handle.mailbox).is_none());
        let actions: Arc<CodeBlockActionsFn> =
            Arc::new(|_, _, _| div().into_any_element());
        {
            let mut mailbox = handle.mailbox.lock().unwrap();
            mailbox.code_block_actions = Some(actions.clone());
            mailbox.result = Some(ParseResult {
                generation: mailbox.generation,
                epoch: mailbox.epoch,
                content: Ok(plain_content("new".into(), TextViewStyle::default(), None)),
                completed_at: Instant::now(),
            });
        }
        let content = UpdateHandle::take_result(&handle.mailbox).unwrap().unwrap();
        assert_eq!(content.root_node.to_markdown(), "new");
        assert!(Arc::ptr_eq(content.node_cx.code_block_actions.as_ref().unwrap(), &actions));
        assert!(UpdateHandle::take_result(&handle.mailbox).is_none());
    }

    #[test]
    fn test_streaming_worker_initial_final_html_and_shutdown() {
        let (handle, wake, result_wake, results) = pipeline("# Initial");
        let mailbox = handle.mailbox.clone();
        handle.wake.try_send(()).unwrap();
        smol::block_on(smol::future::zip(
            parse_updates(mailbox.clone(), wake, result_wake),
            async move {
                results.recv().await.unwrap();
                let initial = UpdateHandle::take_result(&mailbox).unwrap().unwrap();
                assert_eq!(initial.root_node.to_markdown(), "# Initial");

                // A stalled foreground gets one result wakeup, not a queue of
                // full trees. Closing the producer must still deliver final.
                for index in 0..1_000 {
                    handle.send(Update::Text(format!("intermediate {index}").into()));
                }
                let mut style = TextViewStyle::default();
                style.is_dark = true;
                style.paragraph_gap = gpui::rems(2.);
                handle.send(Update::Style(style.clone()));
                handle.update_context(TextViewType::Html, &HighlightTheme::default_light());
                handle.send(Update::Text("<p>Final <strong>HTML</strong></p>".into()));
                drop(handle);
                results.recv().await.unwrap();
                assert!(results.len() <= 1);
                let final_content = UpdateHandle::take_result(&mailbox).unwrap().unwrap();
                let markdown = final_content.root_node.to_markdown();
                assert!(markdown.contains("Final"));
                assert!(markdown.contains("HTML"));
                assert!(!markdown.contains("<p>"));
                assert!(same_style(&final_content.node_cx.style, &style));
                assert!(results.recv().await.is_err());
            },
        ));
    }

    #[test]
    fn test_streaming_plain_fallback_preserves_source_and_native_state() {
        let source: SharedString = "# Heading\n\n```rust\nlet x = 1;\n```\nλ".into();
        let content = plain_content(source.clone(), TextViewStyle::default(), None);
        let node::Node::Root { children } = &content.root_node else { panic!("root") };
        let node::Node::Paragraph(paragraph) = &children[0] else { panic!("paragraph") };
        assert_eq!(paragraph.children[0].text, source);
        let (handle, _, _, _) = pipeline(source.as_str());
        handle.send(Update::Text(source));
        assert_eq!(handle.mailbox.lock().unwrap().generation, 0);
    }

    #[test]
    fn test_streaming_fresh_heading_callbacks_do_not_reparse() {
        let (handle, wake, _, _) = pipeline("# Heading");
        // Simulate a parse finishing with the original callback while render
        // builders allocate fresh callbacks on every frame.
        handle.mailbox.lock().unwrap().result = Some(ParseResult {
            generation: 0,
            epoch: 0,
            content: Ok(plain_content("# Heading".into(), TextViewStyle::default(), None)),
            completed_at: Instant::now(),
        });
        for size in 1..=1_000 {
            let style = TextViewStyle::default()
                .heading_font_size(move |_, _| px(size as f32));
            handle.send(Update::Style(style));
        }
        assert_eq!(handle.mailbox.lock().unwrap().generation, 0);
        assert!(wake.is_empty());
        let content = UpdateHandle::take_result(&handle.mailbox).unwrap().unwrap();
        let heading_font_size = content.node_cx.style.heading_font_size.unwrap();
        assert_eq!(heading_font_size(1, px(14.)), px(1_000.));
    }

    #[test]
    fn test_streaming_append_progress_is_monotonic_and_replacement_invalidates() {
        let (handle, _, _, _) = pipeline("a");
        // Each parse finishes after another append has already arrived. Every
        // compatible prefix must still commit, rather than starving until EOF.
        for generation in 0..100 {
            handle.send(Update::Text("a".repeat(generation as usize + 2).into()));
            {
                let mut mailbox = handle.mailbox.lock().unwrap();
                mailbox.result = Some(ParseResult {
                    generation,
                    epoch: 0,
                    content: Ok(plain_content(
                        "a".repeat(generation as usize + 1).into(),
                        TextViewStyle::default(),
                        None,
                    )),
                    completed_at: Instant::now(),
                });
            }
            let content = UpdateHandle::take_result(&handle.mailbox).unwrap().unwrap();
            assert_eq!(content.root_node.to_markdown().len(), generation as usize + 1);
        }
        {
            let mut mailbox = handle.mailbox.lock().unwrap();
            mailbox.result = Some(ParseResult {
                generation: 98,
                epoch: 0,
                content: Ok(plain_content("older".into(), TextViewStyle::default(), None)),
                completed_at: Instant::now(),
            });
        }
        assert!(UpdateHandle::take_result(&handle.mailbox).is_none());
        handle.send(Update::Text("replacement".into()));
        {
            let mut mailbox = handle.mailbox.lock().unwrap();
            mailbox.result = Some(ParseResult {
                generation: 100,
                epoch: 0,
                content: Ok(plain_content("old prefix".into(), TextViewStyle::default(), None)),
                completed_at: Instant::now(),
            });
        }
        assert!(UpdateHandle::take_result(&handle.mailbox).is_none());
        {
            let mut mailbox = handle.mailbox.lock().unwrap();
            mailbox.result = Some(ParseResult {
                generation: mailbox.generation,
                epoch: mailbox.epoch,
                content: Ok(plain_content("replacement".into(), TextViewStyle::default(), None)),
                completed_at: Instant::now(),
            });
        }
        let final_content = UpdateHandle::take_result(&handle.mailbox).unwrap().unwrap();
        assert_eq!(final_content.root_node.to_markdown(), "replacement");
        let previous_epoch = handle.mailbox.lock().unwrap().epoch;
        let mut style = TextViewStyle::default();
        style.is_dark = true;
        handle.send(Update::Style(style));
        {
            let mut mailbox = handle.mailbox.lock().unwrap();
            mailbox.result = Some(ParseResult {
                generation: mailbox.generation - 1,
                epoch: previous_epoch,
                content: Ok(plain_content("old style".into(), TextViewStyle::default(), None)),
                completed_at: Instant::now(),
            });
        }
        assert!(UpdateHandle::take_result(&handle.mailbox).is_none());
    }

    #[test]
    #[ignore = "manual parse-versus-first-layout control; prints metadata only"]
    fn streaming_markdown_first_layout_benchmark() {
        let source: SharedString = ("# Heading\n\nA **formatted** paragraph with [link](https://example.com).\n\n\
            ```json\n{\"stream\":true,\"items\":[1,2,3],\"message\":\"hello\"}\n```\n\n")
            .repeat(1_000)
            .into();
        let theme = HighlightTheme::default_light();
        // Warm the language query cache, as in a running application.
        let _ = parse_content(TextViewType::Markdown, "```json\n{}\n```",
            TextViewStyle::default(), &theme, &None).unwrap();
        let parse_start = Instant::now();
        let parsed = parse_content(TextViewType::Markdown, &source,
            TextViewStyle::default(), &theme, &None).unwrap();
        let parse_us = parse_start.elapsed().as_micros();
        let plain_start = Instant::now();
        let plain = plain_content(source.clone(), TextViewStyle::default(), None);
        let plain_us = plain_start.elapsed().as_micros();
        std::hint::black_box((&parsed, &plain));
        eprintln!("markdown first-layout control bytes={} paragraphs=1000 parse_highlight_us={} plain_native_us={} update_slots=1 result_slots=1 in_flight=1",
            source.len(), parse_us, plain_us);
    }

    #[test]
    fn test_text_view_state_selection_bounds() {
        assert_eq!(
            selection_bounds(None, None, Default::default()),
            Bounds::default()
        );
        assert_eq!(
            selection_bounds(None, Some(point(px(10.), px(20.))), Default::default()),
            Bounds::default()
        );
        assert_eq!(
            selection_bounds(Some(point(px(10.), px(20.))), None, Default::default()),
            Bounds::default()
        );

        // 10,10 start
        //   |------|
        //   |      |
        //   |------|
        //         50,50
        assert_eq!(
            selection_bounds(
                Some(point(px(10.), px(10.))),
                Some(point(px(50.), px(50.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        // 10,10
        //   |------|
        //   |      |
        //   |------|
        //         50,50 start
        assert_eq!(
            selection_bounds(
                Some(point(px(50.), px(50.))),
                Some(point(px(10.), px(10.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        //        50,10 start
        //   |------|
        //   |      |
        //   |------|
        // 10,50
        assert_eq!(
            selection_bounds(
                Some(point(px(50.), px(10.))),
                Some(point(px(10.), px(50.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        //        50,10
        //   |------|
        //   |      |
        //   |------|
        // 10,50 start
        assert_eq!(
            selection_bounds(
                Some(point(px(10.), px(50.))),
                Some(point(px(50.), px(10.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
    }
}
