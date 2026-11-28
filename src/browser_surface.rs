use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Context, CursorStyle, Entity,
    EventEmitter, FocusHandle, Global, InteractiveElement as _, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Pixels, Point, Render, RenderImage, ScrollEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, canvas, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};
use serde_json::{Value, json};
use smol::channel::TrySendError;
use url::{Host, Url};

use crate::browser_engine::{
    self, AgentGuard, Event, LatestFrames, MAX_FRAME_PIXELS, MAX_FRAME_SIDE, Outbound,
};
use crate::workspace_state::SavedBrowser;

mod automation;
use automation::{Automation, PageOwner};

const MAX_TABS: usize = 16;
const MAX_URL_BYTES: usize = 8192;
static NEXT_TAB: AtomicU64 = AtomicU64::new(1);
const PAGE_KEYS: &str = "EmbeddedBrowserPage";
const COMMAND_RESERVE: usize = 8;

type Viewport = (u32, u32, f32);

fn browser_viewport(width: f64, height: f64, display_scale: f32) -> Viewport {
    let pixel_scale = (f64::from(MAX_FRAME_PIXELS) / (width * height)).sqrt();
    let side_scale = f64::from(MAX_FRAME_SIDE) / width.max(height);
    let limit = pixel_scale.min(side_scale);
    // CEF clamps device scale to 1, so a larger viewport needs fewer CSS pixels.
    let layout_scale = limit.min(1.0);
    (
        (width * layout_scale).floor().max(1.0) as u32,
        (height * layout_scale).floor().max(1.0) as u32,
        display_scale.min((limit as f32).next_down()).max(1.0),
    )
}

gpui::actions!(embedded_browser, [NextField, PreviousField]);

struct PageBindings;
impl Global for PageBindings {}

pub(crate) struct Changed;

struct EngineRef(Weak<Engine>);
impl Global for EngineRef {}

struct Engine {
    outbox: smol::channel::Sender<Outbound>,
    routes: RefCell<HashMap<u64, (WeakEntity<BrowserSurface>, AnyWindowHandle)>>,
    frames: Arc<LatestFrames>,
    stopped: Cell<bool>,
}

impl Engine {
    fn get(cx: &mut App) -> Rc<Self> {
        if let Some(engine) = cx
            .try_global::<EngineRef>()
            .and_then(|slot| slot.0.upgrade())
            && !engine.stopped.get()
        {
            return engine;
        }
        let frames = Arc::new(LatestFrames::default());
        let (outbox, inbox) = browser_engine::start(cx, frames.clone());
        let engine = Rc::new(Self {
            outbox,
            routes: RefCell::new(HashMap::new()),
            frames,
            stopped: Cell::new(false),
        });
        let weak = Rc::downgrade(&engine);
        cx.set_global(EngineRef(weak.clone()));
        cx.spawn(async move |cx| {
            while let Ok(event) = inbox.recv().await {
                let Some(engine) = weak.upgrade() else { return };
                let tab_id = match &event {
                    Event::FrameReady(tab_id) => Some(*tab_id),
                    Event::Message(value) => value
                        .get("tab_id")
                        .or_else(|| value.get("tab").and_then(|tab| tab.get("tab_id")))
                        .and_then(Value::as_u64),
                    Event::Error(error) => {
                        cx.update(|cx| engine.fail(error, cx));
                        return;
                    }
                };
                let Some((view, window)) =
                    tab_id.and_then(|id| engine.routes.borrow().get(&id).cloned())
                else {
                    continue;
                };
                match event {
                    Event::FrameReady(tab_id) => {
                        let frames = engine.frames.clone();
                        let _ = window.update(cx, |_, window, _| {
                            window.on_next_frame(move |window, cx| {
                                if let Some(image) = frames.take(tab_id) {
                                    let _ = view.update(cx, |view, cx| {
                                        view.frame(tab_id, image, window, cx);
                                    });
                                }
                            });
                        });
                    }
                    Event::Message(mut message) => {
                        let _ = window.update(cx, |_, window, cx| {
                            let _ = view.update(cx, |view, cx| {
                                if !view.browser_tool_message(&mut message) {
                                    view.receive(&message, window, cx);
                                }
                            });
                        });
                    }
                    Event::Error(_) => unreachable!(),
                }
            }
            if let Some(engine) = weak.upgrade() {
                cx.update(|cx| engine.fail("The browser engine stopped.", cx));
            }
        })
        .detach();
        engine
    }

    fn fail(&self, error: &str, cx: &mut App) {
        self.stopped.set(true);
        self.outbox.close();
        self.frames.clear();
        for (view, _) in self.routes.take().into_values() {
            let _ = view.update(cx, |view, cx| view.engine_failed(error, cx));
        }
    }

    fn send(self: &Rc<Self>, msg: Value, cx: &mut App) {
        if !matches!(
            self.outbox.try_send(Outbound::Manual(msg)),
            Err(TrySendError::Full(_))
        ) {
            return;
        }
        let weak = Rc::downgrade(self);
        cx.defer(move |cx| {
            if let Some(engine) = weak.upgrade()
                && !engine.stopped.get()
            {
                engine.fail("The browser input queue is full. Reload to reconnect.", cx);
            }
        });
    }

    fn send_agent(&self, msg: Value, guard: Arc<AgentGuard>) -> Result<(), String> {
        self.outbox
            .try_send(Outbound::Agent(msg, guard))
            .map_err(|error| match error {
                TrySendError::Full(_) => "The browser input queue is full.".into(),
                TrySendError::Closed(_) => "The browser engine stopped.".into(),
            })
    }

    fn show(self: &Rc<Self>, previous: Option<u64>, next: Option<u64>, cx: &mut App) {
        if let Some(visible) = self.frames.show(previous, next) {
            self.send(json!({"type": "set_visible_tabs", "tab_ids": visible}), cx);
        }
    }

    fn close_tab(self: &Rc<Self>, id: u64, cx: &mut App) {
        self.show(Some(id), None, cx);
        self.routes.borrow_mut().remove(&id);
        self.send(json!({"type": "close_tab", "tab_id": id}), cx);
    }
}

struct Page {
    id: u64,
    url: String,
    title: SharedString,
    loading: bool,
    back: bool,
    forward: bool,
    open: bool,
    opening_viewport: Option<Viewport>,
    error: Option<SharedString>,
    cursor: CursorStyle,
    owner: PageOwner,
    document: u64,
    revision: u64,
    web_document: bool,
}

impl Page {
    fn new(url: String) -> Self {
        Self {
            id: NEXT_TAB.fetch_add(1, Ordering::Relaxed),
            url,
            title: "New tab".into(),
            loading: false,
            back: false,
            forward: false,
            open: false,
            opening_viewport: None,
            error: None,
            cursor: CursorStyle::Arrow,
            owner: PageOwner::Manual,
            document: 0,
            revision: 0,
            web_document: false,
        }
    }

    fn interactive(&self) -> bool {
        self.open && self.opening_viewport.is_none()
    }
}

pub(crate) struct BrowserSurface {
    window: AnyWindowHandle,
    input: Entity<InputState>,
    focus: FocusHandle,
    pages: Vec<Page>,
    selected: usize,
    saved_active: u64,
    automation: Automation,
    mounted: bool,
    visible: bool,
    bounds: Bounds<Pixels>,
    metrics: Option<Viewport>,
    engine: Option<Rc<Engine>>,
    frame: Option<Arc<RenderImage>>,
    address_error: Option<SharedString>,
    pressed: [bool; 3],
    _input_subscription: gpui::Subscription,
    _window_visibility: gpui::Subscription,
    _release: gpui::Subscription,
}

impl EventEmitter<Changed> for BrowserSurface {}

impl BrowserSurface {
    pub(crate) fn new(saved: SavedBrowser, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if cx.try_global::<PageBindings>().is_none() {
            cx.bind_keys([
                gpui::KeyBinding::new("tab", NextField, Some(PAGE_KEYS)),
                gpui::KeyBinding::new("shift-tab", PreviousField, Some(PAGE_KEYS)),
            ]);
            cx.set_global(PageBindings);
        }
        let mut pages: Vec<_> = saved
            .tabs
            .into_iter()
            .take(MAX_TABS)
            .map(|url| {
                Page::new(if explicit_web_url(&url).is_some() {
                    url
                } else {
                    String::new()
                })
            })
            .collect();
        if pages.is_empty() {
            pages.push(Page::new(String::new()));
        }
        let selected = saved.active.min(pages.len() - 1);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter URL"));
        input.update(cx, |input, cx| {
            input.set_value(pages[selected].url.clone(), window, cx)
        });
        let input_subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Focus => this.reclaim(this.selected, cx),
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                _ => {}
            });
        let view = cx.weak_entity();
        let window_visibility = window.observe_window_visibility(move |visibility, _, cx| {
            let _ = view.update(cx, |this, cx| {
                this.update_visibility(visibility.is_visible(), cx)
            });
        });
        let release = cx.on_release(|this, cx| {
            this.automation.cancel("The Browser panel closed.");
            if let Some(image) = this.frame.take() {
                cx.defer(move |cx| cx.drop_image(image, None));
            }
            if let Some(engine) = this.engine.take() {
                for page in &this.pages {
                    if page.open {
                        engine.close_tab(page.id, cx);
                    }
                }
            }
        });
        Self {
            window: window.window_handle(),
            input,
            focus: cx.focus_handle(),
            saved_active: pages[selected].id,
            automation: Automation::default(),
            pages,
            selected,
            mounted: false,
            visible: false,
            bounds: Bounds::default(),
            metrics: None,
            engine: None,
            frame: None,
            address_error: None,
            pressed: [false; 3],
            _input_subscription: input_subscription,
            _window_visibility: window_visibility,
            _release: release,
        }
    }

    pub(crate) fn snapshot(&self) -> SavedBrowser {
        let mut saved = SavedBrowser::default();
        for page in self.pages.iter().filter(|page| page.owner.is_manual()) {
            if page.id == self.saved_active {
                saved.active = saved.tabs.len();
            }
            saved.tabs.push(page.url.clone());
        }
        saved
    }

    pub(crate) fn focus_address(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    pub(crate) fn set_mounted(&mut self, mounted: bool, window: &Window, cx: &mut Context<Self>) {
        self.mounted = mounted;
        self.update_visibility(window.is_visible(), cx);
    }

    fn update_visibility(&mut self, window_visible: bool, cx: &mut Context<Self>) {
        let visible = self.mounted && window_visible;
        if self.visible == visible {
            return;
        }
        if !visible {
            self.automation
                .cancel("The Browser panel is no longer visible.");
        }
        self.visible = visible;
        if self.pages[self.selected].open {
            let id = self.pages[self.selected].id;
            if let Some(engine) = &self.engine {
                if visible {
                    engine.show(None, Some(id), cx);
                } else {
                    engine.show(Some(id), None, cx);
                }
            }
        }
        if !visible {
            self.metrics = None;
            self.pressed = [false; 3];
            if let Some(image) = self.frame.take() {
                cx.defer(move |cx| cx.drop_image(image, None));
            }
        }
        cx.notify();
    }

    fn clear_frame(&mut self, cx: &mut App, window: Option<&mut Window>) {
        if let Some(image) = self.frame.take() {
            cx.drop_image(image, window);
        }
    }

    fn frame(
        &mut self,
        id: u64,
        image: Arc<RenderImage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.visible && self.pages[self.selected].id == id {
            if let Some(previous) = self.frame.replace(image) {
                let _ = window.drop_image(previous);
            }
            cx.notify();
        }
    }

    fn engine_failed(&mut self, error: &str, cx: &mut Context<Self>) {
        if self.engine.take().is_none() {
            return;
        }
        self.automation.cancel("The browser engine stopped.");
        for page in self.pages.iter_mut().filter(|page| page.open) {
            page.open = false;
            page.opening_viewport = None;
            page.loading = false;
            page.back = false;
            page.forward = false;
            page.error = Some(error.to_owned().into());
        }
        self.clear_frame(cx, None);
        cx.notify();
    }

    fn viewport(&mut self, bounds: Bounds<Pixels>, window: &Window, cx: &mut Context<Self>) {
        self.bounds = bounds;
        let w = f64::from(f32::from(bounds.size.width)).floor();
        let h = f64::from(f32::from(bounds.size.height)).floor();
        if !self.visible {
            return;
        }
        if w < 1.0 || h < 1.0 {
            if self.metrics.take().is_some() {
                self.automation
                    .cancel("The Browser viewport is no longer available.");
            }
            return;
        }
        let geometry = browser_viewport(w, h, window.scale_factor());
        let previous = self.metrics.replace(geometry);
        if previous != Some(geometry) {
            self.automation.wake(self.pages[self.selected].id);
        }
        let page = &self.pages[self.selected];
        if page.interactive() {
            if previous != Some(geometry) {
                self.resize(page.id, geometry, cx);
            }
        } else if !page.open && page.error.is_none() {
            self.open(cx);
        }
    }

    fn resize(&self, id: u64, (width, height, scale): Viewport, cx: &mut App) {
        self.send(
            json!({"type":"resize", "tab_id":id, "width_pt":width,
                "height_pt":height, "scale":scale}),
            cx,
        );
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        if !self.pages[self.selected].owner.is_agent() {
            let _ = self.open_page(None, cx);
        }
    }

    fn open_page(
        &mut self,
        guard: Option<Arc<AgentGuard>>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some((width, height, scale)) = self.metrics.filter(|_| self.visible) else {
            return Ok(());
        };
        let page = &mut self.pages[self.selected];
        if page.open || page.url.is_empty() {
            return Ok(());
        }
        let engine = self.engine.get_or_insert_with(|| Engine::get(cx));
        let id = page.id;
        let ephemeral = !page.owner.is_manual();
        let msg = json!({"type":"open_tab", "tab_id":id, "url":page.url,
            "width_pt":width, "height_pt":height, "scale":scale,
            "focused":false, "file_roots":[], "agent_driven":ephemeral,
            "context":{"kind":if ephemeral { "private" } else { "default" }}});
        if let Some(guard) = guard {
            engine.send_agent(msg, guard)?;
        } else {
            engine.send(msg, cx);
        }
        engine
            .routes
            .borrow_mut()
            .insert(id, (cx.weak_entity(), self.window));
        page.open = true;
        page.opening_viewport = Some((width, height, scale));
        page.document = 0;
        page.web_document = false;
        page.loading = true;
        engine.show(None, Some(id), cx);
        cx.notify();
        Ok(())
    }

    fn send(&self, msg: Value, cx: &mut App) {
        if let Some(engine) = &self.engine {
            engine.send(msg, cx);
        }
    }

    fn action(&mut self, kind: &str, cx: &mut Context<Self>) {
        let page = &mut self.pages[self.selected];
        if page.open {
            let id = page.id;
            self.send(json!({"type":kind, "tab_id":id}), cx);
        } else if kind == "reload" {
            page.error = None;
            self.open(cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reclaim(self.selected, cx);
        let value = self.input.read(cx).value();
        let url = match resolve_address(&value) {
            Ok(url) => url,
            Err(error) => {
                self.address_error = Some(error.into());
                cx.notify();
                return;
            }
        };
        self.address_error = None;
        let page = &mut self.pages[self.selected];
        let id = page.id;
        let open = page.open;
        page.url = url;
        page.error = None;
        page.loading = true;
        self.sync_address(window, cx);
        if open {
            self.send(
                json!({"type":"navigate", "tab_id":id, "url":self.pages[self.selected].url}),
                cx,
            );
        } else {
            self.open(cx);
        }
        self.focus.focus(window, cx);
        if self.pages[self.selected].interactive() {
            self.action("focus_tab", cx);
        }
        cx.emit(Changed);
        cx.notify();
    }

    fn sync_address(&self, window: &mut Window, cx: &mut Context<Self>) {
        let url = &self.pages[self.selected].url;
        if self.input.read(cx).value().as_str() != url {
            self.input
                .update(cx, |input, cx| input.set_value(url.clone(), window, cx));
        }
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.automation.cancel("The user switched Browser tabs.");
        self.reclaim(index, cx);
        self.select_page(index, true, window, cx);
    }

    fn select_page(
        &mut self,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected != index {
            if let Some(engine) = &self.engine {
                engine.show(Some(self.pages[self.selected].id), None, cx);
            }
            self.clear_frame(cx, Some(window));
        }
        self.selected = index;
        if self.pages[index].owner.is_manual() {
            self.saved_active = self.pages[index].id;
        }
        self.pressed = [false; 3];
        self.address_error = None;
        self.sync_address(window, cx);
        let page = &self.pages[index];
        let focus = focus && !page.owner.is_agent();
        if page.url.is_empty() {
            if focus {
                self.focus_address(window, cx);
            }
        } else if self.visible {
            if focus {
                self.focus.focus(window, cx);
            }
            if page.open {
                if let Some(engine) = &self.engine {
                    engine.show(None, Some(page.id), cx);
                }
                if page.interactive() {
                    if let Some(geometry) = self.metrics {
                        self.resize(page.id, geometry, cx);
                    }
                    if focus {
                        self.action("focus_tab", cx);
                    }
                }
            } else if focus && page.error.is_none() {
                self.open(cx);
            }
        }
        cx.emit(Changed);
        cx.notify();
    }

    fn add(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.pages.len() < MAX_TABS {
            self.pages.push(Page::new(url));
            self.select(self.pages.len() - 1, window, cx);
        }
    }

    fn close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_page(index, true, window, cx);
    }

    fn close_page(
        &mut self,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.automation
            .cancel_tab(self.pages[index].id, "The Browser tab closed.");
        let page = self.pages.remove(index);
        if page.open
            && let Some(engine) = &self.engine
        {
            engine.close_tab(page.id, cx);
        }
        if self.pages.iter().all(|page| !page.open) {
            self.engine = None;
        }
        if self.pages.is_empty() {
            self.pages.push(Page::new(String::new()));
        }
        if index == self.selected {
            self.clear_frame(cx, Some(window));
            self.selected = if focus {
                index.min(self.pages.len() - 1)
            } else {
                self.pages
                    .iter()
                    .position(|page| page.id == self.saved_active)
                    .unwrap_or_else(|| index.min(self.pages.len() - 1))
            };
            self.select_page(self.selected, focus, window, cx);
        } else {
            self.selected -= usize::from(index < self.selected);
            cx.emit(Changed);
            cx.notify();
        }
    }

    fn receive(&mut self, message: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let kind = message["type"].as_str().unwrap_or_default();
        let data = if kind == "tab_update" {
            &message["tab"]
        } else {
            message
        };
        let Some(index) = self
            .pages
            .iter()
            .position(|page| data["tab_id"].as_u64() == Some(page.id))
        else {
            return;
        };
        let active = self.selected == index;
        let id = self.pages[index].id;
        let is_manual = self.pages[index].owner.is_manual();
        if kind == "tab_update"
            && self.pages[index].owner.is_agent()
            && (data["context"]["kind"] != "private" || data["agent_driven"] != true)
        {
            self.automation
                .cancel_tab(id, "The engine did not open an isolated agent tab.");
            self.close_page(index, false, window, cx);
            return;
        }
        let page = &mut self.pages[index];
        match kind {
            "tab_update" => {
                let opened_at = page.opening_viewport.take();
                let reported_url = data["url"].as_str().and_then(explicit_web_url);
                let url = reported_url.filter(|url| *url != page.url);
                let url_changed = url.is_some();
                let document = data["document"].as_u64().unwrap_or_default();
                let web_document = reported_url.is_some();
                if url_changed || document != page.document || web_document != page.web_document {
                    page.revision = page.revision.wrapping_add(1);
                }
                page.document = document;
                page.web_document = web_document;
                if let Some(url) = url {
                    page.url = url.to_owned();
                }
                let title = data["title"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        if page.url.is_empty() {
                            "New tab"
                        } else {
                            &page.url
                        }
                    });
                let end = title
                    .char_indices()
                    .nth(120)
                    .map_or(title.len(), |(index, _)| index);
                let title = &title[..end];
                if page.title.as_str() != title {
                    page.title = title.to_owned().into();
                }
                page.back = data["can_go_back"].as_bool().unwrap_or(false);
                page.forward = data["can_go_forward"].as_bool().unwrap_or(false);
                page.loading = data["loading"].as_bool().unwrap_or(false);
                page.error = data["load_error"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("Could not load this page: {s}").into());
                page.cursor = match data["cursor"].as_str() {
                    Some("pointer") => CursorStyle::PointingHand,
                    Some("text") => CursorStyle::IBeam,
                    Some("crosshair") => CursorStyle::Crosshair,
                    _ => CursorStyle::Arrow,
                };
                if url_changed {
                    if active && !self.input.read(cx).focus_handle().is_focused(window) {
                        self.sync_address(window, cx);
                    }
                    cx.emit(Changed);
                }
                if let Some(opened_at) = opened_at.filter(|_| active && self.visible) {
                    if let Some(geometry) = self.metrics.filter(|&geometry| geometry != opened_at) {
                        self.resize(id, geometry, cx);
                    }
                    if !self.pages[index].owner.is_agent() && self.focus.is_focused(window) {
                        self.action("focus_tab", cx);
                    }
                }
            }
            "popup_open" if active && page.owner.is_manual() => {
                if let Some(url) = data["url"].as_str().and_then(explicit_web_url) {
                    self.add(url.to_owned(), window, cx);
                }
                return;
            }
            "refused" => {
                page.loading = false;
                page.error = Some("Chromium refused the navigation.".into());
            }
            "tab_closed" | "download_only_tab" => {
                self.close_page(index, is_manual, window, cx);
                return;
            }
            "copied_text"
                if active
                    && self.visible
                    && !page.owner.is_agent()
                    && self.focus.is_focused(window) =>
            {
                if let Some(text) = data["text"].as_str() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                }
                return;
            }
            "context_menu" => {
                let id = page.id;
                self.send(
                    json!({"type":"context_menu_action", "tab_id":id,
                    "id":data["menu"]["id"], "action":null}),
                    cx,
                );
                return;
            }
            _ => return,
        }
        self.automation.wake(id);
        cx.notify();
    }

    fn coordinates(&self, point: Point<Pixels>) -> (f32, f32) {
        let (x, y) = (
            f32::from(point.x - self.bounds.origin.x),
            f32::from(point.y - self.bounds.origin.y),
        );
        let Some((width, height, _)) = self.metrics else {
            return (x, y);
        };
        (
            x * width as f32 / f32::from(self.bounds.size.width).max(1.0),
            y * height as f32 / f32::from(self.bounds.size.height).max(1.0),
        )
    }

    fn send_motion(&self, make_message: impl FnOnce(u64) -> Value) -> bool {
        let page = &self.pages[self.selected];
        if !page.interactive() {
            return false;
        }
        // Motion is lossy under pressure; leave room for clicks, keys and tab commands.
        if let Some(engine) = &self.engine
            && !engine.outbox.is_closed()
            && engine.outbox.capacity().is_some_and(|capacity| {
                engine.outbox.len() < capacity.saturating_sub(COMMAND_RESERVE)
            })
        {
            let _ = engine
                .outbox
                .try_send(Outbound::Manual(make_message(page.id)));
        }
        true
    }

    fn mouse_move(&self, point: Point<Pixels>, modifiers: Modifiers, leave: bool) {
        // Passive pointer motion over an agent-owned page must not compete with the agent.
        if self.pages[self.selected].owner.is_agent() {
            return;
        }
        self.send_motion(|id| {
            let (x, y) = self.coordinates(point);
            json!({"type":"mouse_move", "tab_id":id,
                "x":x,"y":y, "mods":mods(modifiers), "leave":leave})
        });
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, name)) = pointer_button(event.button) else {
            return;
        };
        self.reclaim(self.selected, cx);
        cx.stop_propagation();
        window.prevent_default();
        self.focus.focus(window, cx);
        if !self.pages[self.selected].interactive() {
            return;
        }
        self.action("focus_tab", cx);
        self.pressed[index] = true;
        let (x, y) = self.coordinates(event.position);
        self.send(
            json!({"type":"mouse_button", "tab_id":self.pages[self.selected].id,
            "x":x, "y":y, "button":name, "up":false, "clicks":event.click_count.clamp(1, 3),
            "mods":mods(event.modifiers)}),
            cx,
        );
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((index, name)) = pointer_button(event.button) else {
            return;
        };
        if !std::mem::take(&mut self.pressed[index]) {
            return;
        }
        let (x, y) = self.coordinates(event.position);
        self.send(
            json!({"type":"mouse_button", "tab_id":self.pages[self.selected].id,
            "x":x, "y":y, "button":name, "up":true, "clicks":event.click_count.clamp(1, 3),
            "mods":mods(event.modifiers)}),
            cx,
        );
    }

    fn tab_key(&mut self, shift: bool, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.reclaim(self.selected, cx);
        let page = &self.pages[self.selected];
        if page.interactive() {
            self.send(
                json!({"type":"key", "tab_id":page.id, "down":true, "named":"Tab",
                "mods":u8::from(shift)}),
                cx,
            );
        }
    }

    fn key(&mut self, key: &Keystroke, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        if down {
            self.reclaim(self.selected, cx);
        } else if self.pages[self.selected].owner.is_agent() {
            return;
        }
        if key.modifiers.control && !key.modifiers.alt {
            let id = self.pages[self.selected].id;
            match key.key.as_str() {
                "l" => {
                    if down {
                        self.focus_address(window, cx);
                    }
                }
                "r" => {
                    if down {
                        self.action("reload", cx);
                    }
                }
                "v" => {
                    if down
                        && self.pages[self.selected].interactive()
                        && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        && text.len() <= 8 * 1024
                    {
                        self.send(json!({"type":"paste_text", "tab_id":id, "text":text}), cx);
                    }
                }
                "c" | "x" => {
                    if down && self.pages[self.selected].interactive() {
                        self.send(
                            json!({"type":"copy_selection", "tab_id":id, "cut":key.key == "x"}),
                            cx,
                        );
                    }
                }
                _ => {
                    self.page_key(key, down, cx);
                    return;
                }
            }
            cx.stop_propagation();
            return;
        }
        self.page_key(key, down, cx);
    }

    fn page_key(&self, key: &Keystroke, down: bool, cx: &mut Context<Self>) {
        let page = &self.pages[self.selected];
        if !page.interactive() {
            return;
        }
        let named = match key.key.as_str() {
            "enter" => Some("Enter"),
            "backspace" => Some("Backspace"),
            "delete" => Some("Delete"),
            "tab" => Some("Tab"),
            "escape" => Some("Escape"),
            "left" => Some("ArrowLeft"),
            "right" => Some("ArrowRight"),
            "up" => Some("ArrowUp"),
            "down" => Some("ArrowDown"),
            "home" => Some("Home"),
            "end" => Some("End"),
            "pageup" => Some("PageUp"),
            "pagedown" => Some("PageDown"),
            _ => None,
        };
        let text = if named.is_some() {
            None
        } else {
            key.key_char.as_deref().or_else(|| {
                (key.key.chars().count() == 1)
                    .then_some(key.key.as_str())
                    .or_else(|| (key.key == "space").then_some(" "))
            })
        };
        if text.is_some() || named.is_some() {
            self.send(
                json!({"type":"key", "tab_id":page.id, "down":down, "named":named,
                "text":text, "mods":mods(key.modifiers)}),
                cx,
            );
            cx.stop_propagation();
        }
    }
}

impl BrowserSurface {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let page = &self.pages[self.selected];
        let loading = page.loading;
        let hover = cx.theme().muted.opacity(0.40);
        let browser = cx.weak_entity();
        let reload = if loading {
            Icon::new(IconName::Close)
        } else {
            Icon::empty().path("ui_icons/refresh.svg")
        };
        div()
            .id("browser-toolbar")
            .h(px(44.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(2.0))
            .px(px(8.0))
            .child(
                browser_button("browser-back", IconName::ArrowLeft)
                    .tooltip("Back")
                    .disabled(!page.back)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reclaim(this.selected, cx);
                        this.action("back", cx);
                    })),
            )
            .child(
                browser_button("browser-forward", IconName::ArrowRight)
                    .tooltip("Forward")
                    .disabled(!page.forward)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reclaim(this.selected, cx);
                        this.action("forward", cx);
                    })),
            )
            .child(
                browser_button("browser-reload", reload)
                    .tooltip(if loading { "Stop loading" } else { "Reload" })
                    .disabled(page.url.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reclaim(this.selected, cx);
                        let loading = this.pages[this.selected].loading;
                        this.action(if loading { "stop" } else { "reload" }, cx);
                    })),
            )
            .child(
                div()
                    .id("browser-address")
                    .debug_selector(|| "browser-address".into())
                    .flex_1()
                    .min_w_0()
                    .mx(px(4.0))
                    .rounded(px(6.0))
                    .hover(move |address| address.bg(hover))
                    .child(
                        Input::new(&self.input)
                            .small()
                            .appearance(false)
                            .px(px(6.0))
                            .text_size(px(14.0)),
                    ),
            )
            .child(
                browser_button("browser-new-page", IconName::Plus)
                    .tooltip("New page")
                    .disabled(self.pages.len() >= MAX_TABS)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.add(String::new(), window, cx)),
                    ),
            )
            .child(
                browser_button("browser-menu", IconName::EllipsisVertical)
                    .tooltip("Browser menu")
                    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                        let focus = browser.clone();
                        let close = browser.clone();
                        menu.item(
                            PopupMenuItem::new("Focus address")
                                .icon(IconName::Search)
                                .on_click(move |_, window, cx| {
                                    let _ = focus.update(cx, |view, cx| {
                                        view.focus_address(window, cx);
                                    });
                                }),
                        )
                        .item(
                            PopupMenuItem::new("Close page")
                                .icon(IconName::Close)
                                .on_click(move |_, window, cx| {
                                    let _ = close.update(cx, |view, cx| {
                                        view.close(view.selected, window, cx);
                                    });
                                }),
                        )
                    }),
            )
    }

    fn render_page_tabs(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let (selected, hover, foreground, muted) = (
            theme.secondary,
            theme.muted.opacity(0.40),
            theme.foreground,
            theme.muted_foreground,
        );
        div()
            .id("browser-tabs")
            .flex()
            .flex_shrink_0()
            .min_w_0()
            .gap(px(3.0))
            .px(px(8.0))
            .pt(px(4.0))
            .overflow_x_scroll()
            .children(self.pages.iter().enumerate().map(|(index, page)| {
                let active = index == self.selected;
                let id = page.id;
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .max_w(px(168.0))
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .text_color(if active { foreground } else { muted })
                    .when(active, |tab| tab.bg(selected))
                    .child(
                        div()
                            .id(("browser-select-tab", page.id))
                            .min_w_0()
                            .pl(px(8.0))
                            .pr(px(3.0))
                            .truncate()
                            .cursor_pointer()
                            .hover(move |title| title.bg(hover))
                            .when(page.owner.is_agent(), |title| title.child("Agent · "))
                            .child(page.title.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(index) =
                                    this.pages.iter().position(|page| page.id == id)
                                {
                                    this.select(index, window, cx);
                                }
                            })),
                    )
                    .child(
                        browser_button(("browser-close-tab", page.id), IconName::Close)
                            .tooltip("Close page")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(index) =
                                    this.pages.iter().position(|page| page.id == id)
                                {
                                    this.close(index, window, cx);
                                }
                            })),
                    )
            }))
    }
}

fn browser_button(id: impl Into<gpui::ElementId>, icon: impl Into<Icon>) -> Button {
    Button::new(id).xsmall().ghost().size(px(24.0)).icon(icon)
}

fn browser_empty_state(cx: &App) -> impl gpui::IntoElement {
    let theme = cx.theme();
    let (border, card, muted) = (theme.border, theme.secondary, theme.muted_foreground);
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .flex()
        .flex_col()
        .items_center()
        .px(px(16.0))
        .pt(px(56.0))
        .text_center()
        .child(
            div()
                .relative()
                .w(px(48.0))
                .h(px(36.0))
                .children([0.0, 16.0].map(|left| {
                    div()
                        .absolute()
                        .top(px(4.0))
                        .left(px(left))
                        .w(px(32.0))
                        .h(px(30.0))
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(border)
                        .bg(card)
                }))
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px(6.0))
                        .size(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(border)
                        .bg(card)
                        .child(
                            Icon::new(IconName::Globe)
                                .with_size(px(17.0))
                                .text_color(muted),
                        ),
                ),
        )
        .child(
            div()
                .mt(px(44.0))
                .text_size(px(20.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("No preview yet"),
        )
        .child(
            div()
                .mt(px(24.0))
                .text_size(px(14.0))
                .text_color(muted)
                .child("Enter a URL above to preview a page, or type a Google search."),
        )
}

impl Render for BrowserSurface {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = cx.theme();
        let (bg, foreground, muted) = (theme.background, theme.foreground, theme.muted_foreground);
        let page = &self.pages[self.selected];
        let empty = page.url.is_empty() && self.frame.is_none();
        let notice: Option<SharedString> = self
            .address_error
            .clone()
            .or_else(|| page.error.clone())
            .or_else(|| {
                if self.frame.is_some() || empty {
                    None
                } else if page.interactive() {
                    Some("Loading page…".into())
                } else {
                    Some("Starting Chromium… First use downloads the browser engine.".into())
                }
            });
        let weak = cx.weak_entity();
        let mut viewport = div().id("browser-page").debug_selector(|| "browser-page".into())
            .key_context(PAGE_KEYS)
            .on_action(cx.listener(|this, _: &NextField, _, cx| this.tab_key(false, cx)))
            .on_action(cx.listener(|this, _: &PreviousField, _, cx| this.tab_key(true, cx)))
            .relative().flex_1().min_h_0().overflow_hidden().track_focus(&self.focus)
            .cursor(page.cursor)
            .child(canvas(move |bounds, window, cx| {
                weak.update(cx, |view, cx| {
                    view.viewport(bounds, window, cx);
                    view.frame.clone()
                }).ok().flatten()
            }, |bounds, image, window, _| {
                if let Some(image) = image {
                    let _ = window.paint_image(bounds, bounds, Default::default(), image, 0, false);
                }
            }).absolute().size_full())
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                this.mouse_move(event.position, event.modifiers, false);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, window, _| {
                if !hovered { this.mouse_move(window.mouse_position(), window.modifiers(), true); }
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                this.reclaim(this.selected, cx);
                if this.send_motion(|id| {
                    let (x, y) = this.coordinates(event.position);
                    let delta = event.delta.pixel_delta(px(40.0));
                    json!({"type":"wheel", "tab_id":id, "x":x, "y":y,
                        "dx":f32::from(delta.x), "dy":f32::from(delta.y), "mods":mods(event.modifiers)})
                }) {
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.key(&event.keystroke, true, window, cx);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, window, cx| {
                this.key(&event.keystroke, false, window, cx);
            }));
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            viewport = viewport
                .on_mouse_down(button, cx.listener(Self::mouse_down))
                .on_mouse_up(button, cx.listener(Self::mouse_up))
                .on_mouse_up_out(button, cx.listener(Self::mouse_up));
        }
        viewport = viewport.when(empty && notice.is_none(), |viewport| {
            viewport.child(browser_empty_state(cx))
        });
        viewport = viewport.children(notice.map(|notice| {
            div()
                .absolute()
                .top_3()
                .left_3()
                .right_3()
                .rounded_md()
                .px_3()
                .py_2()
                .text_sm()
                .bg(bg)
                .text_color(muted)
                .child(notice)
        }));

        let tabs =
            (self.pages.len() > 1 || page.owner.is_agent()).then(|| self.render_page_tabs(cx));
        let toolbar = self.render_toolbar(cx);
        div()
            .id("browser-surface")
            .debug_selector(|| "browser-surface".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(foreground)
            .children(tabs)
            .child(toolbar)
            .child(viewport)
    }
}

fn pointer_button(button: MouseButton) -> Option<(usize, &'static str)> {
    match button {
        MouseButton::Left => Some((0, "primary")),
        MouseButton::Right => Some((1, "secondary")),
        MouseButton::Middle => Some((2, "middle")),
        _ => None,
    }
}

fn mods(m: Modifiers) -> u8 {
    u8::from(m.shift)
        | (u8::from(m.control) << 1)
        | (u8::from(m.alt) << 2)
        | (u8::from(m.platform) << 3)
}

fn explicit_web_url(address: &str) -> Option<&str> {
    parse_web_url(address).map(|_| address)
}

fn parse_web_url(address: &str) -> Option<Url> {
    if address.len() > MAX_URL_BYTES {
        return None;
    }
    let url = Url::parse(address).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn resolve_address(input: &str) -> Result<String, &'static str> {
    const INVALID_URL: &str = "Enter an HTTP or HTTPS URL without credentials.";
    const TOO_LONG: &str = "The address or search query is too long.";

    let address = input.trim();
    if address.is_empty() {
        return Err("Search Google or enter a URL.");
    }
    if address.len() > MAX_URL_BYTES {
        return Err(TOO_LONG);
    }

    let (scheme, after_scheme) = address.split_once(':').unwrap_or_default();
    let authority = address.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    let url = if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        parse_web_url(address).ok_or(INVALID_URL)?
    } else if address.starts_with(['/', '\\'])
        || (after_scheme.starts_with("//") && !scheme.contains(char::is_whitespace))
        || [
            "file",
            "javascript",
            "data",
            "blob",
            "about",
            "mailto",
            "view-source",
        ]
        .iter()
        .any(|blocked| scheme.eq_ignore_ascii_case(blocked))
    {
        return Err(INVALID_URL);
    } else if !address.contains(char::is_whitespace)
        && (host.trim_end_matches('.').contains('.')
            || host.eq_ignore_ascii_case("localhost")
            || (authority.starts_with('[') && authority.contains(':'))
            || authority.contains('@')
            || port.parse::<u16>().is_ok())
    {
        let mut url = parse_web_url(&format!("https://{address}")).ok_or(INVALID_URL)?;
        if matches!(url.host(), Some(Host::Domain("localhost")))
            || matches!(url.host(), Some(Host::Ipv4(ip)) if ip.is_loopback())
            || matches!(url.host(), Some(Host::Ipv6(ip)) if ip.is_loopback())
        {
            url.set_scheme("http").map_err(|_| INVALID_URL)?;
        }
        url
    } else {
        let mut search =
            Url::parse("https://www.google.com/search").expect("valid search endpoint");
        search.query_pairs_mut().append_pair("q", address);
        search
    };

    if url.as_str().len() > MAX_URL_BYTES {
        return Err(TOO_LONG);
    }
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn browser_viewport_keeps_normal_layout_and_caps_large_paints() {
        use super::*;
        assert_eq!(browser_viewport(800.0, 600.0, 2.0), (800, 600, 2.0));
        let retina = browser_viewport(1600.0, 1200.0, 2.0);
        assert_eq!((retina.0, retina.1), (1600, 1200));
        assert!(retina.2 > 1.0 && retina.2 < 2.0);
        for (width, height, dpi) in [
            (1600.0, 1200.0, 2.0),
            (3840.0, 2160.0, 1.0),
            (6912.0, 720.0, 1.0),
            (1100.0, 1100.0, 1.75),
        ] {
            let (w, h, scale) = browser_viewport(width, height, dpi);
            let painted_w = (f64::from(w) * f64::from(scale)).ceil() as u32;
            let painted_h = (f64::from(h) * f64::from(scale)).ceil() as u32;
            assert!(painted_w.max(painted_h) <= MAX_FRAME_SIDE);
            assert!(painted_w * painted_h <= MAX_FRAME_PIXELS + 2 * MAX_FRAME_SIDE + 1);
        }
    }

    #[test]
    fn browser_address_bar_routes_urls_and_google_searches() {
        for (input, expected) in [
            (" example.com/docs ", Some("https://example.com/docs")),
            ("localhost:5173", Some("http://localhost:5173/")),
            ("[::1]:8080", Some("http://[::1]:8080/")),
            ("http://example.com", Some("http://example.com/")),
            ("intranet:8443", Some("https://intranet:8443/")),
            ("javascript:alert(1)", None),
            ("file:///etc/passwd", None),
            ("https://name:secret@example.com", None),
            ("name:secret@example.com", None),
            ("file:/home/dev/private notes.txt", None),
            ("ftp://example.com", None),
            ("  ", None),
            ("shob", Some("https://www.google.com/search?q=shob")),
            (
                " Rust & C++ #docs ",
                Some("https://www.google.com/search?q=Rust+%26+C%2B%2B+%23docs"),
            ),
            (
                "site:example.com rust",
                Some("https://www.google.com/search?q=site%3Aexample.com+rust"),
            ),
        ] {
            assert_eq!(
                super::resolve_address(input).ok().as_deref(),
                expected,
                "{input}"
            );
        }
        assert!(super::resolve_address(&"é".repeat(2000)).is_err());
    }

    #[test]
    fn browser_startup_keeps_motion_out_and_sends_latest_size() {
        use super::*;
        use crate::resize_tests::TestAppContext;
        use gpui::{MouseDownEvent, VisualContext as _, point, size};

        let mut app = TestAppContext::new();
        let (mut app, root) = crate::resize_tests::open_app(&mut app);
        app.update_window_entity(&root, |_, window, cx| {
            let (outbox, commands) = smol::channel::bounded(16);
            let engine = Rc::new(Engine {
                outbox,
                routes: RefCell::new(HashMap::new()),
                frames: Arc::new(LatestFrames::default()),
                stopped: Cell::new(false),
            });
            cx.set_global(EngineRef(Rc::downgrade(&engine)));
            let view = cx.new(|cx| {
                BrowserSurface::new(
                    SavedBrowser {
                        tabs: vec!["https://example.com/".into()],
                        active: 0,
                    },
                    window,
                    cx,
                )
            });
            view.update(cx, |view, cx| {
                let origin = point(px(0.), px(0.));
                let bounds = |width| Bounds::new(origin, size(px(width), px(240.)));
                view.set_mounted(true, window, cx);
                view.viewport(bounds(320.), window, cx);
                let value = |outbound| match outbound {
                    Outbound::Manual(value) | Outbound::Agent(value, _) => value,
                };
                let recv = || value(commands.try_recv().unwrap());
                let open = recv();
                assert_eq!(open["url"], "https://example.com/");
                assert_eq!(open["width_pt"], 320);
                assert_eq!(recv()["type"], "set_visible_tabs");

                for width in 321..360 {
                    view.mouse_move(origin, Modifiers::none(), false);
                    view.viewport(bounds(width as f32), window, cx);
                }
                let click = MouseDownEvent {
                    button: MouseButton::Left,
                    position: origin,
                    click_count: 1,
                    ..Default::default()
                };
                view.mouse_down(&click, window, cx);
                assert!(matches!(
                    commands.try_recv(),
                    Err(smol::channel::TryRecvError::Empty)
                ));

                view.receive(
                    &json!({"type": "tab_update", "tab": {
                        "tab_id": open["tab_id"], "url": open["url"], "loading": true
                    }}),
                    window,
                    cx,
                );
                let resize = recv();
                assert_eq!(resize["type"], "resize");
                assert_eq!(resize["width_pt"], 359);
                assert_eq!(recv()["type"], "focus_tab");

                for _ in 0..32 {
                    view.mouse_move(origin, Modifiers::none(), false);
                }
                view.mouse_down(&click, window, cx);
                let queued: Vec<_> =
                    std::iter::from_fn(|| commands.try_recv().ok().map(value)).collect();
                assert_eq!(queued[queued.len() - 2]["type"], "focus_tab");
                assert_eq!(queued.last().unwrap()["type"], "mouse_button");

                view.update_visibility(false, cx);
                assert_eq!(recv()["tab_ids"], json!([]));
                view.update_visibility(true, cx);
                assert_eq!(recv()["tab_ids"], json!([open["tab_id"]]));
            });
        });
    }
}
