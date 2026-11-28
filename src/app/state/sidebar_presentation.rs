use gpui::{Anchor, DispatchPhase, Element, GlobalElementId, InspectorElementId, LayoutId};
use std::cell::RefCell;
use std::time::{Duration, Instant};

const SIDEBAR_TOOLTIP_HOVER: Duration = Duration::from_millis(600);
const SIDEBAR_TOOLTIP_REOPEN: Duration = Duration::from_millis(400);
const SIDEBAR_TOOLTIP_GRACE: Duration = Duration::from_millis(100);
const SIDEBAR_ROW_MOTION: Duration = Duration::from_millis(150);
const SIDEBAR_MOTION_LIMIT: usize = 40;
const SIDEBAR_MOTION_LAYOUT_LIMIT: usize = 1_024;
const SIDEBAR_MOTION_TRAVEL: Pixels = px(40.0);

#[derive(Default)]
struct SidebarPresentation {
    tooltip: Rc<RefCell<SidebarTooltipState>>,
    motion: Rc<RefCell<SidebarMotionState>>,
}

#[derive(Clone, Copy, Default)]
enum SidebarTooltipSide {
    #[default]
    Top,
    Right,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTooltipIntent {
    Pending,
    Hover,
    Keyboard,
}

struct SidebarTooltipAnchor {
    bounds: Bounds<Pixels>,
    label: SharedString,
    side: SidebarTooltipSide,
    background: Hsla,
    foreground: Hsla,
    border: Hsla,
}

impl SidebarTooltipAnchor {
    fn draw(self, window: &mut Window, cx: &mut App) {
        let gap = px(4.0);
        let (tooltip, trigger, offset) = match self.side {
            SidebarTooltipSide::Top => (Anchor::BottomCenter, Anchor::TopCenter, gpui::point(px(0.0), -gap)),
            SidebarTooltipSide::Right => (Anchor::LeftCenter, Anchor::RightCenter, gpui::point(gap, px(0.0))),
            SidebarTooltipSide::Bottom => (Anchor::TopCenter, Anchor::BottomCenter, gpui::point(px(0.0), gap)),
        };
        let width = (window.viewport_size().width - px(10.0)).clamp(px(1.0), px(320.0));
        let popup = div()
            .id("sidebar-tooltip")
            .debug_selector(|| "sidebar-tooltip".into())
            .role(gpui::Role::Tooltip)
            .max_w(width)
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(self.border)
            .bg(self.background)
            .text_color(self.foreground)
            .text_xs()
            .child(self.label);
        let mut overlay = gpui::anchored()
            .anchor(tooltip)
            .attach(self.bounds, trigger)
            .offset(offset)
            .snap_to_window_with_margin(px(5.0))
            .child(popup)
            .into_any_element();
        let available = size(gpui::AvailableSpace::MaxContent, gpui::AvailableSpace::MaxContent);
        overlay.layout_as_root(available, window, cx);
        window.defer_draw(overlay, Point::default(), 140, None);
    }
}

#[derive(Default)]
struct SidebarTooltipState {
    owner: Option<(ElementId, SidebarTooltipIntent)>,
    keyboard: Option<ElementId>,
    last_closed: Option<Instant>,
    timer: Option<gpui::Task<()>>,
    blocked_at: Option<Point<Pixels>>,
    seen_owner: bool,
    anchor: Option<SidebarTooltipAnchor>,
}

impl SidebarTooltipState {
    fn intent(&self, key: &ElementId) -> Option<SidebarTooltipIntent> {
        self.owner.as_ref().filter(|(owner, _)| owner == key).map(|(_, intent)| *intent)
    }

    fn warm(&self, now: Instant) -> bool {
        self.owner.as_ref().is_some_and(|(_, intent)| *intent != SidebarTooltipIntent::Pending)
            || self.last_closed.is_some_and(|closed| now.saturating_duration_since(closed) < SIDEBAR_TOOLTIP_REOPEN)
    }

    fn show(&mut self, key: ElementId, intent: SidebarTooltipIntent) {
        self.timer = None;
        self.owner = Some((key, intent));
        self.anchor = None;
        self.seen_owner = false;
    }

    fn close(&mut self, now: Instant) -> bool {
        self.anchor = None;
        self.seen_owner = false;
        if self.owner.take().is_some_and(|(_, intent)| intent != SidebarTooltipIntent::Pending) {
            self.last_closed = Some(now);
            true
        } else {
            false
        }
    }

    fn keyboard_open(&self) -> bool {
        self.owner.as_ref().is_some_and(|(owner, intent)| {
            *intent == SidebarTooltipIntent::Keyboard && self.keyboard.as_ref() == Some(owner)
        })
    }

    fn dismiss(&mut self, pointer: Point<Pixels>, keep_keyboard: bool, now: Instant) -> bool {
        if keep_keyboard && self.keyboard_open() {
            return false;
        }
        self.blocked_at = Some(pointer);
        self.timer = None;
        self.close(now)
    }
}

fn sidebar_tooltip_timer(
    shared: &Rc<RefCell<SidebarTooltipState>>,
    delay: Duration,
    open: bool,
    window: &Window,
    cx: &App,
) -> gpui::Task<()> {
    let weak = Rc::downgrade(shared);
    let timer = cx.background_executor().timer(delay);
    window.spawn(cx, async move |cx| {
        timer.await;
        let _ = cx.update(|window, cx| {
            let Some(shared) = weak.upgrade() else { return };
            let mut state = shared.borrow_mut();
            if open {
                if let Some((_, intent @ SidebarTooltipIntent::Pending)) = state.owner.as_mut() {
                    *intent = SidebarTooltipIntent::Hover;
                    window.refresh();
                }
            } else if state.close(cx.background_executor().now()) {
                window.refresh();
            }
        });
    })
}

fn sidebar_tooltip_hover(
    shared: &Rc<RefCell<SidebarTooltipState>>,
    key: &ElementId,
    hovered: bool,
    window: &mut Window,
    cx: &App,
) {
    let mut state = shared.borrow_mut();
    if !hovered {
        match state.intent(key) {
            Some(SidebarTooltipIntent::Pending) => {
                state.timer = None;
                state.close(cx.background_executor().now());
            }
            Some(SidebarTooltipIntent::Hover) => {
                state.timer = Some(sidebar_tooltip_timer(shared, SIDEBAR_TOOLTIP_GRACE, false, window, cx));
            }
            _ => {}
        }
        return;
    }
    if !window.is_window_active() || window.last_input_was_keyboard() || state.blocked_at.is_some() {
        return;
    }
    match state.intent(key) {
        Some(SidebarTooltipIntent::Hover) => {
            state.timer = None;
            return;
        }
        Some(SidebarTooltipIntent::Pending) => return,
        Some(SidebarTooltipIntent::Keyboard) | None => {}
    }
    if state.warm(cx.background_executor().now()) {
        state.show(key.clone(), SidebarTooltipIntent::Hover);
        window.refresh();
    } else {
        state.show(key.clone(), SidebarTooltipIntent::Pending);
        state.timer = Some(sidebar_tooltip_timer(shared, SIDEBAR_TOOLTIP_HOVER, true, window, cx));
    }
}

struct SidebarTooltipTrigger {
    child: AnyElement,
    shared: Rc<RefCell<SidebarTooltipState>>,
    key: ElementId,
    label: SharedString,
    side: SidebarTooltipSide,
    background: Hsla,
    foreground: Hsla,
    border: Hsla,
}

struct SidebarTooltipArea {
    child: AnyElement,
    shared: Rc<RefCell<SidebarTooltipState>>,
}

struct SidebarMotionList {
    child: AnyElement,
    shared: Rc<RefCell<SidebarMotionState>>,
    app: gpui::WeakEntity<AppState>,
    tokens: ThemeTokens,
    animate: bool,
    shelves: u8,
}

struct SidebarMotionRow {
    child: AnyElement,
    shared: Rc<RefCell<SidebarMotionState>>,
    session_id: usize,
}

// The four wrappers pass their child's layout node through unchanged. In
// particular, they add no flex boxes or IDs around existing hit targets.
macro_rules! sidebar_transparent_element {
    () => {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> { None }

        fn source_location(&self) -> Option<&'static std::panic::Location<'static>> { None }
    };
}

impl Element for SidebarTooltipTrigger {
    sidebar_transparent_element!();

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
        let child_focus = self.child.prepaint(window, cx);
        let visible = !bounds.intersect(&window.content_mask().bounds).is_empty();
        let focused = visible && window.is_window_active() && window.last_input_was_keyboard() && child_focus.is_some();
        let mut state = self.shared.borrow_mut();
        if focused && state.keyboard.as_ref() != Some(&self.key) {
            state.keyboard = Some(self.key.clone());
            state.show(self.key.clone(), SidebarTooltipIntent::Keyboard);
        } else if !focused && state.keyboard.as_ref() == Some(&self.key) {
            state.keyboard = None;
            if state.intent(&self.key) == Some(SidebarTooltipIntent::Keyboard) {
                state.timer = Some(sidebar_tooltip_timer(&self.shared, SIDEBAR_TOOLTIP_GRACE, false, window, cx));
            }
        }
        if let Some(intent) = state.intent(&self.key).filter(|_| visible) {
            state.seen_owner = true;
            if intent != SidebarTooltipIntent::Pending {
                state.anchor = Some(SidebarTooltipAnchor {
                    bounds,
                    label: self.label.clone(),
                    side: self.side,
                    background: self.background,
                    foreground: self.foreground,
                    border: self.border,
                });
            }
        }
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
        self.child.paint(window, cx);
    }
}

impl Element for SidebarTooltipArea {
    sidebar_transparent_element!();

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
        {
            let mut state = self.shared.borrow_mut();
            state.anchor = None;
            state.seen_owner = false;
            if !window.is_window_active() {
                state.timer = None;
                state.keyboard = None;
                state.close(cx.background_executor().now());
            }
        }
        self.child.prepaint(window, cx);
        let anchor = {
            let mut state = self.shared.borrow_mut();
            if !state.seen_owner {
                state.timer = None;
                state.keyboard = None;
                state.close(cx.background_executor().now());
                None
            } else {
                state.anchor.take()
            }
        };
        if let Some(anchor) = anchor {
            anchor.draw(window, cx);
        }
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
        let shared = self.shared.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture
                && shared.borrow_mut().dismiss(event.position, false, cx.background_executor().now())
            {
                window.refresh();
            }
        });
        let shared = self.shared.clone();
        window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let mut state = shared.borrow_mut();
                // A retained scroll can otherwise move the trigger without
                // updating the separately painted, keyboard-owned tooltip.
                if state.dismiss(event.position, true, cx.background_executor().now())
                    || state.keyboard_open()
                {
                    window.refresh();
                }
            }
        });
        let shared = self.shared.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, _| {
            if phase == DispatchPhase::Capture {
                let mut state = shared.borrow_mut();
                if state.blocked_at.is_some_and(|position| position != event.position) {
                    state.blocked_at = None;
                }
            }
        });
        self.child.paint(window, cx);
    }
}

#[derive(Default)]
struct SidebarMotionState {
    layouts: Vec<(usize, LayoutId)>,
    collect_layouts: bool,
    layout_overflow: bool,
    positions: HashMap<usize, SidebarRowPosition>,
    next_positions: HashMap<usize, SidebarRowPosition>,
    live: HashMap<usize, SidebarRowAnimation>,
    exits: Vec<SidebarRowExit>,
    previous: Option<(Point<Pixels>, u8)>,
    now: Option<Instant>,
}

struct SidebarRowPosition {
    bounds: Bounds<Pixels>,
    preview: Option<SidebarRowPreview>,
}

struct SidebarRowPreview {
    title: SharedString,
    project: SharedString,
    logo: SharedString,
    bot_avatar: Option<bot_avatar::AvatarConfig>,
    selected: bool,
}

#[derive(Clone, Copy)]
struct SidebarRowAnimation {
    started: Instant,
    offset: Pixels,
    opacity: f32,
}

impl SidebarRowAnimation {
    fn progress(self, now: Instant) -> f32 {
        let linear = (now.saturating_duration_since(self.started).as_secs_f32()
            / SIDEBAR_ROW_MOTION.as_secs_f32()).min(1.0);
        1.0 - (1.0 - linear) * (1.0 - linear)
    }

    fn finished(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= SIDEBAR_ROW_MOTION
    }

    fn sample(self, now: Instant) -> (Pixels, f32) {
        let remaining = 1.0 - self.progress(now);
        (self.offset * remaining, 1.0 - (1.0 - self.opacity) * remaining)
    }
}

struct SidebarRowExit {
    session_id: usize,
    row: SidebarRowPreview,
    bounds: Bounds<Pixels>,
    motion: SidebarRowAnimation,
}

impl SidebarRowExit {
    fn element(&self, tokens: &ThemeTokens, now: Instant) -> (Point<Pixels>, Div) {
        let progress = self.motion.progress(now);
        let opacity = self.motion.opacity * (1.0 - progress);
        let position = self.bounds.origin + gpui::point(px(0.0), self.motion.offset * progress);
        let slim = self.bounds.size.height <= SIDEBAR_THREAD_SLIM_HEIGHT;
        let title = div().min_w_0().flex_1().truncate().text_sm()
            .font_weight(gpui::FontWeight::MEDIUM).child(self.row.title.clone());
        let logo = self.row.bot_avatar.map_or_else(
            || session_logo_icon(self.row.logo.clone(), tokens.sidebar_icon),
            |avatar| sidebar_bot_icon(bot_avatar::avatar(avatar, SIDEBAR_BOT_ART_SIZE)).into_any_element(),
        );
        let element = div()
            .w(self.bounds.size.width)
            .h(self.bounds.size.height)
            .px(px(if self.row.bot_avatar.is_some() { 9.0 } else { 10.0 }))
            .rounded(px(6.0))
            .overflow_hidden()
            .flex()
            .opacity(opacity)
            .text_color(if self.row.selected { tokens.sidebar_item_active_fg } else { tokens.sidebar_fg })
            .when(self.row.selected, |row| row.bg(tokens.sidebar_item_active))
            .map(|row| if slim {
                if self.row.bot_avatar.is_some() {
                    row.items_center().gap(px(10.0)).child(logo).child(title)
                } else {
                    row.items_center().gap(px(8.0)).child(title).child(logo)
                }
            } else {
                row.flex_col().justify_center().gap(px(3.0)).child(
                    div().flex().items_center().gap(px(6.0)).text_xs().text_color(tokens.sidebar_muted)
                        .child(logo).child(div().min_w_0().truncate().child(self.row.project.clone())),
                ).child(title.flex_none())
            });
        (position, element)
    }
}

impl SidebarMotionState {
    fn reset(&mut self) {
        self.layouts.clear();
        self.positions.clear();
        self.next_positions.clear();
        self.live.clear();
        self.exits.clear();
        self.previous = None;
        self.now = None;
    }

    fn update(
        &mut self,
        origin: Point<Pixels>,
        shelves: u8,
        now: Instant,
        app: Option<&AppState>,
        window: &mut Window,
    ) {
        self.now = Some(now);
        let clip = window.content_mask().bounds;
        let visible_top = clip.top() - SIDEBAR_MOTION_TRAVEL;
        let visible_bottom = clip.bottom() + SIDEBAR_MOTION_TRAVEL;
        // Rows have positive, vertically ordered bounds in the inbox (including
        // nested shelves). Skip off-screen history without measuring every row.
        let start = self.layouts.partition_point(|&(_, layout)| window.layout_bounds(layout).bottom() <= visible_top);
        self.next_positions.clear();
        for &(id, layout) in &self.layouts[start..] {
            let mut bounds = window.layout_bounds(layout);
            if bounds.top() >= visible_bottom {
                break;
            }
            if bounds.is_empty() {
                continue;
            }
            if self.next_positions.len() == SIDEBAR_MOTION_LIMIT {
                self.reset();
                return;
            }
            bounds.origin -= origin;
            let preview = app.and_then(|app| app.sidebar_motion_preview(id));
            self.next_positions.insert(id, SidebarRowPosition { bounds, preview });
        }
        // Local positions are independent of the scroll offset. Scrolling
        // changes the set of tracked rows, but is not itself a row transition.
        let same_view = self.previous == Some((origin, shelves));
        if !same_view {
            self.live.clear();
            self.exits.clear();
        } else {
            self.live.retain(|_, animation| !animation.finished(now));
            self.exits.retain(|exit| {
                !exit.motion.finished(now) && !self.next_positions.contains_key(&exit.session_id)
            });
            let changes = self.positions.keys().filter(|id| !self.next_positions.contains_key(id)).count()
                + self.next_positions.keys().filter(|id| !self.positions.contains_key(id)).count();
            if changes > SIDEBAR_MOTION_LIMIT {
                self.live.clear();
                self.exits.clear();
            } else {
                for (&id, previous) in &mut self.positions {
                    if self.next_positions.contains_key(&id) {
                        continue;
                    }
                    let (offset, opacity) = self.live.remove(&id).map_or((px(0.0), 1.0), |animation| animation.sample(now));
                    if self.exits.len() < SIDEBAR_MOTION_LIMIT && let Some(row) = previous.preview.take() {
                        let mut bounds = previous.bounds;
                        bounds.origin += origin + gpui::point(px(0.0), offset);
                        self.exits.push(SidebarRowExit {
                            session_id: id,
                            row,
                            bounds,
                            motion: SidebarRowAnimation { started: now, offset: bounds.size.height.min(SIDEBAR_MOTION_TRAVEL), opacity },
                        });
                    }
                }
                for (&id, next) in &self.next_positions {
                    let Some(previous) = self.positions.get(&id) else {
                        self.live.insert(id, SidebarRowAnimation {
                            started: now,
                            offset: -next.bounds.size.height.min(SIDEBAR_MOTION_TRAVEL),
                            opacity: 0.0,
                        });
                        continue;
                    };
                    let movement = previous.bounds.top() - next.bounds.top();
                    if movement != px(0.0) {
                        let (remaining, opacity) = self.live.get(&id).map_or((px(0.0), 1.0), |animation| animation.sample(now));
                        self.live.insert(id, SidebarRowAnimation { started: now, offset: movement + remaining, opacity });
                    }
                }
            }
        }
        self.previous = Some((origin, shelves));
        std::mem::swap(&mut self.positions, &mut self.next_positions);
    }
}

impl Element for SidebarMotionList {
    sidebar_transparent_element!();

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        {
            let mut motion = self.shared.borrow_mut();
            motion.collect_layouts = self.animate && !cx.reduce_motion();
            motion.layouts.clear();
            motion.layout_overflow = false;
        }
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
        let animate = self.animate && !cx.reduce_motion() && !self.shared.borrow().layout_overflow;
        if animate {
            let app = self.app.upgrade();
            self.shared.borrow_mut().update(
                bounds.origin,
                self.shelves,
                cx.background_executor().now(),
                app.as_ref().map(|app| app.read(cx)),
                window,
            );
        } else {
            self.shared.borrow_mut().reset();
        }
        self.child.prepaint(window, cx);
        let motion = self.shared.borrow();
        if let Some(now) = motion.now {
            if !motion.live.is_empty() || !motion.exits.is_empty() {
                window.request_animation_frame();
            }
            let mask = window.content_mask();
            for exit in &motion.exits {
                let (origin, element) = exit.element(&self.tokens, now);
                let mut element = element.into_any_element();
                element.layout_as_root(exit.bounds.size.into(), window, cx);
                window.defer_draw(element, origin, 1, Some(mask));
            }
        }
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
        self.child.paint(window, cx);
    }
}

impl Element for SidebarMotionRow {
    sidebar_transparent_element!();

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let layout = self.child.request_layout(window, cx);
        let mut motion = self.shared.borrow_mut();
        if motion.collect_layouts {
            if motion.layouts.len() < SIDEBAR_MOTION_LAYOUT_LIMIT {
                motion.layouts.push((self.session_id, layout));
            } else {
                // Disable this pass rather than extrapolate from a partial layout.
                motion.collect_layouts = false;
                motion.layout_overflow = true;
            }
        }
        (layout, ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
        let animated = {
            let motion = self.shared.borrow();
            if motion.live.is_empty() {
                None
            } else {
                motion.now.zip(motion.live.get(&self.session_id)).map(|(now, animation)| animation.sample(now))
            }
        };
        if let Some((offset, opacity)) = animated {
            if opacity < 1.0 && let Some(row) = self.child.downcast_mut::<gpui::Stateful<Div>>() {
                row.style().opacity = Some(opacity);
            }
            window.with_element_offset(gpui::point(px(0.0), offset), |window| self.child.prepaint(window, cx));
        } else {
            self.child.prepaint(window, cx);
        }
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
        self.child.paint(window, cx);
    }
}

impl AppState {
    fn sidebar_tooltip(
        &self,
        trigger: gpui::Stateful<Div>,
        key: impl Into<ElementId>,
        label: impl Into<SharedString>,
        side: SidebarTooltipSide,
        tokens: ThemeTokens,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = key.into();
        let shared = self.sidebar_presentation.tooltip.clone();
        let trigger = trigger.on_hover({
            let shared = shared.clone();
            let key = key.clone();
            move |hovered, window, cx| sidebar_tooltip_hover(&shared, &key, *hovered, window, cx)
        });
        SidebarTooltipTrigger {
            child: trigger.into_any_element(),
            shared,
            key,
            label: label.into(),
            side,
            background: tokens.card_bg,
            foreground: tokens.text_primary,
            border: tokens.sidebar_border,
        }.into_any_element()
    }

    fn sidebar_tooltip_area(&self, content: impl IntoElement) -> AnyElement {
        SidebarTooltipArea { child: content.into_any_element(), shared: self.sidebar_presentation.tooltip.clone() }
            .into_any_element()
    }

    fn reset_sidebar_tooltip(&self) {
        *self.sidebar_presentation.tooltip.borrow_mut() = SidebarTooltipState::default();
    }

    fn sidebar_motion_list(
        &self,
        list: impl IntoElement,
        animate: bool,
        shelves: u8,
        tokens: ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        SidebarMotionList {
            child: list.into_any_element(),
            shared: self.sidebar_presentation.motion.clone(),
            app: cx.weak_entity(),
            tokens,
            animate,
            shelves,
        }.into_any_element()
    }

    fn sidebar_motion_row(&self, row: gpui::Stateful<Div>, session_id: usize) -> AnyElement {
        SidebarMotionRow {
            child: row.into_any_element(),
            shared: self.sidebar_presentation.motion.clone(),
            session_id,
        }.into_any_element()
    }

    fn reset_sidebar_motion(&self) {
        self.sidebar_presentation.motion.borrow_mut().reset();
    }

    fn sidebar_motion_preview(&self, session_id: usize) -> Option<SidebarRowPreview> {
        let &(project_index, session_index) = self.session_index.get(&session_id)?;
        let project = self.projects.get(project_index)?;
        let session = project.sessions.get(session_index)?;
        Some(SidebarRowPreview {
            title: self.sidebar_thread_title(session).clone(),
            project: project.name.clone(),
            logo: session.logo.clone(),
            bot_avatar: self.bots.get(&session.uid).map(|bot| bot.avatar),
            selected: self.sidebar_active_thread_key() == Some((project_index, session_index)),
        })
    }
}

#[cfg(test)]
mod sidebar_presentation_tests {
    use super::*;

    #[test]
    fn only_displayed_tooltips_warm_the_shared_provider() {
        let start = Instant::now();
        let mut state = SidebarTooltipState::default();
        state.show("one".into(), SidebarTooltipIntent::Pending);
        assert!(!state.warm(start));
        assert!(!state.close(start));
        assert!(!state.warm(start));

        state.show("one".into(), SidebarTooltipIntent::Hover);
        assert!(state.warm(start));
        assert!(state.close(start));
        assert!(state.warm(start + Duration::from_millis(399)));
        assert!(!state.warm(start + Duration::from_millis(400)));
    }

    #[test]
    fn scrolling_preserves_a_keyboard_tooltip_but_pointer_down_dismisses_it() {
        let now = Instant::now();
        let mut state = SidebarTooltipState::default();
        let key = ElementId::from("keyboard-trigger");
        state.keyboard = Some(key.clone());
        state.show(key.clone(), SidebarTooltipIntent::Keyboard);
        let position = gpui::point(px(12.0), px(20.0));

        assert!(!state.dismiss(position, true, now));
        assert!(state.intent(&key) == Some(SidebarTooltipIntent::Keyboard));
        assert!(state.dismiss(position, false, now));
        assert!(state.owner.is_none());
        assert_eq!(state.blocked_at, Some(position));
    }

    #[test]
    fn row_ease_out_settles_within_a_single_transition() {
        let started = Instant::now();
        let motion = SidebarRowAnimation { started, offset: px(-40.0), opacity: 0.0 };
        assert_eq!(motion.sample(started), (px(-40.0), 0.0));
        let midpoint = started + Duration::from_millis(75);
        let (offset, opacity) = motion.sample(midpoint);
        assert!(offset > px(-20.0));
        assert!(opacity > 0.5);
        assert!(!motion.finished(midpoint));
        assert_eq!(motion.sample(started + SIDEBAR_ROW_MOTION), (px(0.0), 1.0));
        assert!(motion.finished(started + SIDEBAR_ROW_MOTION));
    }
}
