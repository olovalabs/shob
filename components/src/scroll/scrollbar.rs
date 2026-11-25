use std::{
    cell::Cell,
    ops::Deref,
    panic::Location,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{ActiveTheme, AxisExt};
use gpui::{
    App, Axis, BorderStyle, Bounds, ContentMask, Anchor, CursorStyle, Edges, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IsZero,
    LayoutId, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    Position, ScrollEvent, ScrollHandle, Size, Style, UniformListScrollHandle, Window,
    fill, point, px, relative, size,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use smol::Timer;

const WIDTH: Pixels = px(4. * 2. + 8.);
const MIN_THUMB_SIZE: f32 = 48.;

const THUMB_WIDTH: Pixels = px(6.);
const THUMB_RADIUS: Pixels = px(6. / 2.);
const THUMB_INSET: Pixels = px(4.);

const THUMB_ACTIVE_WIDTH: Pixels = px(8.);
const THUMB_ACTIVE_RADIUS: Pixels = px(8. / 2.);
const THUMB_ACTIVE_INSET: Pixels = px(4.);

const FADE_OUT_DURATION: f32 = 3.0;
const FADE_OUT_DELAY: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default, JsonSchema)]
pub enum ScrollbarShow {

    #[default]
    Scrolling,

    Hover,

    Always,
}

impl ScrollbarShow {
    fn is_hover(&self) -> bool {
        matches!(self, Self::Hover)
    }

    fn is_always(&self) -> bool {
        matches!(self, Self::Always)
    }
}

pub trait ScrollbarHandle: 'static {
    /// Get the current offset of the scroll handle.
    fn offset(&self) -> Point<Pixels>;
    /// Set the offset of the scroll handle.
    fn set_offset(&self, offset: Point<Pixels>);
    /// The full size of the content, including padding.
    fn content_size(&self) -> Size<Pixels>;
    /// Move the thumb during a drag. Distances are positive, in the range captured at grab time.
    fn drag_to(&self, axis: Axis, _previous: Pixels, target: Pixels, _range: Pixels) {
        let mut offset = self.offset();
        if axis.is_vertical() {
            offset.y = -target;
        } else {
            offset.x = -target;
        }
        self.set_offset(offset);
    }
    /// Called after a track click or drag changes the offset.
    fn on_scroll(&self, _cx: &mut App) {}
    /// Called when start dragging the scrollbar thumb.
    fn start_drag(&self) {}
    /// Called when end dragging the scrollbar thumb.
    fn end_drag(&self) {}
}

impl ScrollbarHandle for ScrollHandle {
    fn offset(&self) -> Point<Pixels> {
        self.offset()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset(offset);
    }

    fn content_size(&self) -> Size<Pixels> {
        Size::from(self.max_offset()) + self.bounds().size
    }
}

impl ScrollbarHandle for UniformListScrollHandle {
    fn offset(&self) -> Point<Pixels> {
        self.0.borrow().base_handle.offset()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.0.borrow_mut().base_handle.set_offset(offset)
    }

    fn content_size(&self) -> Size<Pixels> {
        let base_handle = &self.0.borrow().base_handle;
        Size::from(base_handle.max_offset()) + base_handle.bounds().size
    }
}

impl ScrollbarHandle for ListState {
    fn offset(&self) -> Point<Pixels> {
        self.scroll_px_offset_for_scrollbar()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset_from_scrollbar(offset);
    }

    fn content_size(&self) -> Size<Pixels> {
        self.viewport_bounds().size + Size::from(self.max_offset_for_scrollbar())
    }

    fn start_drag(&self) {
        self.scrollbar_drag_started();
    }

    fn end_drag(&self) {
        self.scrollbar_drag_ended();
    }
}

#[doc(hidden)]
#[derive(Debug, Clone)]
struct ScrollbarState(Rc<Cell<ScrollbarStateInner>>);

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
struct ScrollbarStateInner {
    hovered_axis: Option<Axis>,
    hovered_on_thumb: Option<Axis>,
    drag: Option<ScrollbarDrag>,
    last_scroll_offset: Point<Pixels>,
    last_scroll_time: Option<Instant>,
    // Last update offset
    last_update: Instant,
    idle_timer_scheduled: bool,
}

#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    axis: Axis,
    grab_offset: Pixels,
    last_pointer: Pixels,
    offset: Pixels,
    range: Pixels,
    thumb_length: Pixels,
}

impl Default for ScrollbarState {
    fn default() -> Self {
        Self(Rc::new(Cell::new(ScrollbarStateInner {
            hovered_axis: None,
            hovered_on_thumb: None,
            drag: None,
            last_scroll_offset: point(px(0.), px(0.)),
            last_scroll_time: None,
            last_update: Instant::now(),
            idle_timer_scheduled: false,
        })))
    }
}

impl Deref for ScrollbarState {
    type Target = Rc<Cell<ScrollbarStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ScrollbarStateInner {
    fn with_hovered(&self, axis: Option<Axis>) -> Self {
        let mut state = *self;
        state.hovered_axis = axis;
        if axis.is_some() {
            state.last_scroll_time = Some(std::time::Instant::now());
        }
        state
    }

    fn with_hovered_on_thumb(&self, axis: Option<Axis>) -> Self {
        let mut state = *self;
        state.hovered_on_thumb = axis;
        if self.is_scrollbar_visible() {
            if axis.is_some() {
                state.last_scroll_time = Some(std::time::Instant::now());
            }
        }
        state
    }

    fn with_last_scroll(
        &self,
        last_scroll_offset: Point<Pixels>,
        last_scroll_time: Option<Instant>,
    ) -> Self {
        let mut state = *self;
        state.last_scroll_offset = last_scroll_offset;
        state.last_scroll_time = last_scroll_time;
        state
    }

    fn with_last_scroll_time(&self, t: Option<Instant>) -> Self {
        let mut state = *self;
        state.last_scroll_time = t;
        state
    }

    fn with_last_update(&self, t: Instant) -> Self {
        let mut state = *self;
        state.last_update = t;
        state
    }

    fn with_idle_timer_scheduled(&self, scheduled: bool) -> Self {
        let mut state = *self;
        state.idle_timer_scheduled = scheduled;
        state
    }

    fn is_scrollbar_visible(&self) -> bool {
        // On drag
        if self.drag.is_some() {
            return true;
        }

        if let Some(last_time) = self.last_scroll_time {
            let elapsed = Instant::now().duration_since(last_time).as_secs_f32();
            elapsed < FADE_OUT_DURATION
        } else {
            false
        }
    }
}

/// Scrollbar axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarAxis {
    /// Vertical scrollbar.
    Vertical,
    /// Horizontal scrollbar.
    Horizontal,
    /// Show both vertical and horizontal scrollbars.
    Both,
}

impl From<Axis> for ScrollbarAxis {
    fn from(axis: Axis) -> Self {
        match axis {
            Axis::Vertical => Self::Vertical,
            Axis::Horizontal => Self::Horizontal,
        }
    }
}

impl ScrollbarAxis {
    /// Return true if the scrollbar axis is vertical.
    #[inline]
    pub fn is_vertical(&self) -> bool {
        matches!(self, Self::Vertical)
    }

    /// Return true if the scrollbar axis is horizontal.
    #[inline]
    pub fn is_horizontal(&self) -> bool {
        matches!(self, Self::Horizontal)
    }

    /// Return true if the scrollbar axis is both vertical and horizontal.
    #[inline]
    pub fn is_both(&self) -> bool {
        matches!(self, Self::Both)
    }

    /// Return true if the scrollbar has vertical axis.
    #[inline]
    pub fn has_vertical(&self) -> bool {
        matches!(self, Self::Vertical | Self::Both)
    }

    /// Return true if the scrollbar has horizontal axis.
    #[inline]
    pub fn has_horizontal(&self) -> bool {
        matches!(self, Self::Horizontal | Self::Both)
    }

    #[inline]
    fn all(&self) -> Vec<Axis> {
        match self {
            Self::Vertical => vec![Axis::Vertical],
            Self::Horizontal => vec![Axis::Horizontal],
            // This should keep Horizontal first, Vertical is the primary axis
            // if Vertical not need display, then Horizontal will not keep right margin.
            Self::Both => vec![Axis::Horizontal, Axis::Vertical],
        }
    }
}

/// Scrollbar control for scroll-area or a uniform-list.
pub struct Scrollbar {
    pub(crate) id: ElementId,
    axis: ScrollbarAxis,
    scrollbar_show: Option<ScrollbarShow>,
    scroll_handle: Rc<dyn ScrollbarHandle>,
    scroll_size: Option<Size<Pixels>>,
    thumb_geometry: Option<(Pixels, Pixels)>,
    /// Optional rate limit for expensive scroll targets.
    max_fps: Option<usize>,
}

impl Scrollbar {
    /// Create a new scrollbar.
    ///
    /// This will have both vertical and horizontal scrollbars.
    #[track_caller]
    pub fn new<H: ScrollbarHandle + Clone>(scroll_handle: &H) -> Self {
        let caller = Location::caller();
        Self {
            id: ElementId::CodeLocation(*caller),
            axis: ScrollbarAxis::Both,
            scrollbar_show: None,
            scroll_handle: Rc::new(scroll_handle.clone()),
            max_fps: Some(120),
            scroll_size: None,
            thumb_geometry: None,
        }
    }

    /// Create with horizontal scrollbar.
    #[track_caller]
    pub fn horizontal<H: ScrollbarHandle + Clone>(scroll_handle: &H) -> Self {
        Self::new(scroll_handle).axis(ScrollbarAxis::Horizontal)
    }

    /// Create with vertical scrollbar.
    #[track_caller]
    pub fn vertical<H: ScrollbarHandle + Clone>(scroll_handle: &H) -> Self {
        Self::new(scroll_handle).axis(ScrollbarAxis::Vertical)
    }

    /// Set a specific element id, default is the [`Location::caller`].
    ///
    /// NOTE: In most cases, you don't need to set a specific id for scrollbar.
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }

    pub fn scrollbar_show(mut self, scrollbar_show: ScrollbarShow) -> Self {
        self.scrollbar_show = Some(scrollbar_show);
        self
    }

    pub fn scroll_size(mut self, scroll_size: Size<Pixels>) -> Self {
        self.scroll_size = Some(scroll_size);
        self
    }

    /// Override the visible thumb width and its inset from the track edge.
    pub fn thumb_geometry(mut self, width: Pixels, inset: Pixels) -> Self {
        self.thumb_geometry = Some((width, inset));
        self
    }

    pub fn axis(mut self, axis: impl Into<ScrollbarAxis>) -> Self {
        self.axis = axis.into();
        self
    }

    /// Apply every pointer update without a wall-clock throttle.
    pub fn unthrottled_drag(mut self) -> Self {
        self.max_fps = None;
        self
    }

    pub(crate) fn max_fps(mut self, max_fps: usize) -> Self {
        self.max_fps = Some(max_fps.clamp(30, 120));
        self
    }

    pub(crate) const fn width() -> Pixels {
        WIDTH
    }

    fn style_for_active(cx: &App) -> (Hsla, Hsla, Hsla, Pixels, Pixels, Pixels) {
        (
            cx.theme().scrollbar_thumb_hover,
            cx.theme().scrollbar,
            cx.theme().border,
            THUMB_ACTIVE_WIDTH,
            THUMB_ACTIVE_INSET,
            THUMB_ACTIVE_RADIUS,
        )
    }

    fn style_for_hovered_thumb(cx: &App) -> (Hsla, Hsla, Hsla, Pixels, Pixels, Pixels) {
        (
            cx.theme().scrollbar_thumb_hover,
            cx.theme().scrollbar,
            cx.theme().border,
            THUMB_ACTIVE_WIDTH,
            THUMB_ACTIVE_INSET,
            THUMB_ACTIVE_RADIUS,
        )
    }

    fn style_for_hovered_bar(cx: &App) -> (Hsla, Hsla, Hsla, Pixels, Pixels, Pixels) {
        (
            cx.theme().scrollbar_thumb,
            cx.theme().scrollbar,
            gpui::transparent_black(),
            THUMB_ACTIVE_WIDTH,
            THUMB_ACTIVE_INSET,
            THUMB_ACTIVE_RADIUS,
        )
    }

    fn style_for_normal(&self, cx: &App) -> (Hsla, Hsla, Hsla, Pixels, Pixels, Pixels) {
        let scrollbar_show = self.scrollbar_show.unwrap_or(cx.theme().scrollbar_show);
        let (width, inset, radius) = match scrollbar_show {
            ScrollbarShow::Scrolling => (THUMB_WIDTH, THUMB_INSET, THUMB_RADIUS),
            _ => (THUMB_ACTIVE_WIDTH, THUMB_ACTIVE_INSET, THUMB_ACTIVE_RADIUS),
        };

        (
            cx.theme().scrollbar_thumb,
            cx.theme().scrollbar,
            gpui::transparent_black(),
            width,
            inset,
            radius,
        )
    }

    fn style_for_idle(&self, cx: &App) -> (Hsla, Hsla, Hsla, Pixels, Pixels, Pixels) {
        let scrollbar_show = self.scrollbar_show.unwrap_or(cx.theme().scrollbar_show);
        let (width, inset, radius) = match scrollbar_show {
            ScrollbarShow::Scrolling => (THUMB_WIDTH, THUMB_INSET, THUMB_RADIUS),
            _ => (THUMB_ACTIVE_WIDTH, THUMB_ACTIVE_INSET, THUMB_ACTIVE_RADIUS),
        };

        (
            gpui::transparent_black(),
            gpui::transparent_black(),
            gpui::transparent_black(),
            width,
            inset,
            radius,
        )
    }
}

#[doc(hidden)]
pub struct PrepaintState {
    hitbox: Hitbox,
    scrollbar_state: ScrollbarState,
    states: Vec<AxisPrepaintState>,
}

#[doc(hidden)]
pub struct AxisPrepaintState {
    axis: Axis,
    bar_hitbox: Hitbox,
    bounds: Bounds<Pixels>,
    radius: Pixels,
    bg: Hsla,
    border: Hsla,
    thumb_bounds: Bounds<Pixels>,

    thumb_fill_bounds: Bounds<Pixels>,
    thumb_bg: Hsla,
    range: Pixels,
    offset: Pixels,
    thumb_length: Pixels,
    thumb_start: Pixels,
    travel: Pixels,
}

fn scrollbar_pointer(axis: Axis, track: Bounds<Pixels>, position: Point<Pixels>) -> Pixels {
    if axis.is_vertical() {
        position.y - track.top()
    } else {
        position.x - track.left()
    }
}

fn move_scrollbar_thumb(
    state: &ScrollbarState,
    handle: &dyn ScrollbarHandle,
    mut drag: ScrollbarDrag,
    track: Bounds<Pixels>,
    travel: Pixels,
    position: Point<Pixels>,
    cx: &mut App,
) -> bool {
    let pointer = scrollbar_pointer(drag.axis, track, position);
    if travel <= px(0.) || pointer == drag.last_pointer {
        return false;
    }
    let offset = drag.range * ((pointer - drag.grab_offset) / travel).clamp(0., 1.);
    if offset == drag.offset {
        return false;
    }
    handle.drag_to(drag.axis, drag.offset, offset, drag.range);
    handle.on_scroll(cx);
    drag.offset = offset;
    drag.last_pointer = pointer;
    let mut current = state.get().with_last_update(Instant::now());
    current.drag = Some(drag);
    state.set(current);
    true
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
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
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        });

        let state = window
            .use_state(cx, |_, _| ScrollbarState::default())
            .read(cx)
            .clone();

        let mut states = vec![];
        let mut has_both = self.axis.is_both();
        let scroll_size = self
            .scroll_size
            .unwrap_or(self.scroll_handle.content_size());

        for axis in self.axis.all().into_iter() {
            let is_vertical = axis.is_vertical();
            let (scroll_area_size, container_size, scroll_position) = if is_vertical {
                (
                    scroll_size.height,
                    hitbox.size.height,
                    self.scroll_handle.offset().y,
                )
            } else {
                (
                    scroll_size.width,
                    hitbox.size.width,
                    self.scroll_handle.offset().x,
                )
            };

            let margin_end = if has_both && !is_vertical {
                WIDTH
            } else {
                px(0.)
            };

            let drag = state.get().drag.filter(|drag| drag.axis == axis);
            let track_length = (container_size - margin_end).max(px(0.));
            let range = drag.map_or(scroll_area_size - container_size, |drag| drag.range);
            if range <= px(0.) || track_length <= px(0.) {
                has_both = false;
                continue;
            }

            let thumb_length = drag
                .map_or_else(
                    || (container_size / scroll_area_size * track_length).max(px(MIN_THUMB_SIZE)),
                    |drag| drag.thumb_length,
                )
                .min(track_length);
            let travel = track_length - thumb_length;
            let offset = drag.map_or(-scroll_position, |drag| drag.offset).clamp(px(0.), range);
            let thumb_start = travel * (offset / range);

            let bounds = Bounds {
                origin: if is_vertical {
                    point(hitbox.origin.x + hitbox.size.width - WIDTH, hitbox.origin.y)
                } else {
                    point(
                        hitbox.origin.x,
                        hitbox.origin.y + hitbox.size.height - WIDTH,
                    )
                },
                size: gpui::Size {
                    width: if is_vertical {
                        WIDTH
                    } else {
                        hitbox.size.width
                    },
                    height: if is_vertical {
                        hitbox.size.height
                    } else {
                        WIDTH
                    },
                },
            };

            let scrollbar_show = self.scrollbar_show.unwrap_or(cx.theme().scrollbar_show);
            let is_always_to_show = scrollbar_show.is_always();
            let is_hover_to_show = scrollbar_show.is_hover();
            let is_hovered_on_bar = state.get().hovered_axis == Some(axis);
            let is_hovered_on_thumb = state.get().hovered_on_thumb == Some(axis);
            let is_offset_changed = state.get().last_scroll_offset != self.scroll_handle.offset();

            let (thumb_bg, bar_bg, bar_border, thumb_width, inset, radius) =
                if drag.is_some() {
                    Self::style_for_active(cx)
                } else if is_hover_to_show && (is_hovered_on_bar || is_hovered_on_thumb) {
                    if is_hovered_on_thumb {
                        Self::style_for_hovered_thumb(cx)
                    } else {
                        Self::style_for_hovered_bar(cx)
                    }
                } else if is_offset_changed {
                    self.style_for_normal(cx)
                } else if is_always_to_show {
                    if is_hovered_on_thumb {
                        Self::style_for_hovered_thumb(cx)
                    } else {
                        Self::style_for_hovered_bar(cx)
                    }
                } else {
                    let mut idle_state = self.style_for_idle(cx);

                    if let Some(last_time) = state.get().last_scroll_time {
                        let elapsed = Instant::now().duration_since(last_time).as_secs_f32();
                        if is_hovered_on_bar {
                            state.set(state.get().with_last_scroll_time(Some(Instant::now())));
                            idle_state = if is_hovered_on_thumb {
                                Self::style_for_hovered_thumb(cx)
                            } else {
                                Self::style_for_hovered_bar(cx)
                            };
                        } else if elapsed < FADE_OUT_DELAY {
                            idle_state.0 = cx.theme().scrollbar_thumb;

                            if !state.get().idle_timer_scheduled {
                                let state = state.clone();
                                state.set(state.get().with_idle_timer_scheduled(true));
                                let current_view = window.current_view();
                                let next_delay = Duration::from_secs_f32(FADE_OUT_DELAY - elapsed);
                                window
                                    .spawn(cx, async move |cx| {
                                        Timer::after(next_delay).await;
                                        state.set(state.get().with_idle_timer_scheduled(false));
                                        cx.update(|_, cx| cx.notify(current_view)).ok();
                                    })
                                    .detach();
                            }
                        } else if elapsed < FADE_OUT_DURATION {
                            let opacity = 1.0 - (elapsed - FADE_OUT_DELAY).powi(10);
                            idle_state.0 = cx.theme().scrollbar_thumb.opacity(opacity);

                            window.request_animation_frame();
                        }
                    }

                    idle_state
                };

            let (thumb_width, inset, radius) = self.thumb_geometry
                .map(|(width, inset)| (width, inset, width / 2.0))
                .unwrap_or((thumb_width, inset, radius));
            let inset = inset.clamp(px(0.), (WIDTH / 2.).min((thumb_length - px(1.)).max(px(0.)) / 2.));
            let painted_length = thumb_length - inset * 2;
            let thumb_bounds = if is_vertical {
                Bounds::from_anchor_and_size(
                    Anchor::TopRight,
                    bounds.top_right() + point(-inset, inset + thumb_start),
                    size(WIDTH - inset, painted_length),
                )
            } else {
                Bounds::from_anchor_and_size(
                    Anchor::BottomLeft,
                    bounds.bottom_left() + point(inset + thumb_start, -inset),
                    size(painted_length, WIDTH - inset),
                )
            };

            let thumb_fill_bounds = if is_vertical {
                Bounds::from_anchor_and_size(
                    Anchor::TopRight,
                    bounds.top_right() + point(-inset, inset + thumb_start),
                    size(thumb_width, painted_length),
                )
            } else {
                Bounds::from_anchor_and_size(
                    Anchor::BottomLeft,
                    bounds.bottom_left() + point(inset + thumb_start, -inset),
                    size(painted_length, thumb_width),
                )
            };

            let bar_hitbox = window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
            });

            states.push(AxisPrepaintState {
                axis,
                bar_hitbox,
                bounds,
                radius,
                bg: bar_bg,
                border: bar_border,
                thumb_bounds,
                thumb_fill_bounds,
                thumb_bg,
                range,
                offset,
                thumb_length,
                thumb_start,
                travel,
            })
        }

        PrepaintState {
            hitbox,
            states,
            scrollbar_state: state,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let scrollbar_state = &prepaint.scrollbar_state;
        let scrollbar_show = self.scrollbar_show.unwrap_or(cx.theme().scrollbar_show);
        let view_id = window.current_view();
        let hitbox_bounds = prepaint.hitbox.bounds;
        let is_visible = scrollbar_state.get().is_scrollbar_visible() || scrollbar_show.is_always();
        let is_hover_to_show = scrollbar_show.is_hover();

        if self.scroll_handle.offset() != scrollbar_state.get().last_scroll_offset {
            scrollbar_state.set(
                scrollbar_state
                    .get()
                    .with_last_scroll(self.scroll_handle.offset(), Some(Instant::now())),
            );
            cx.notify(view_id);
        }

        window.with_content_mask(
            Some(ContentMask {
                bounds: hitbox_bounds,
            }),
            |window| {
                for state in prepaint.states.iter() {
                    let axis = state.axis;
                    let mut radius = state.radius;
                    if cx.theme().radius.is_zero() {
                        radius = px(0.);
                    }
                    let bounds = state.bounds;
                    let thumb_bounds = state.thumb_bounds;
                    let range = state.range;
                    let offset = state.offset;
                    let thumb_length = state.thumb_length;
                    let thumb_start = state.thumb_start;
                    let travel = state.travel;
                    let is_vertical = axis.is_vertical();

                    window.set_cursor_style(CursorStyle::default(), &state.bar_hitbox);

                    window.paint_layer(hitbox_bounds, |cx| {
                        cx.paint_quad(fill(state.bounds, state.bg));

                        cx.paint_quad(PaintQuad {
                            bounds,
                            corner_radii: (0.).into(),
                            background: gpui::transparent_black().into(),
                            border_widths: if is_vertical {
                                Edges {
                                    top: px(0.),
                                    right: px(0.),
                                    bottom: px(0.),
                                    left: px(0.),
                                }
                            } else {
                                Edges {
                                    top: px(0.),
                                    right: px(0.),
                                    bottom: px(0.),
                                    left: px(0.),
                                }
                            },
                            border_color: state.border,
                            border_style: BorderStyle::default(),
                        });

                        cx.paint_quad(
                            fill(state.thumb_fill_bounds, state.thumb_bg).corner_radii(radius),
                        );
                    });

                    window.on_mouse_event({
                        let state = scrollbar_state.clone();
                        let scroll_handle = self.scroll_handle.clone();

                        move |event: &ScrollEvent, phase, _, cx| {
                            if phase.bubble() && hitbox_bounds.contains(&event.position) {
                                if scroll_handle.offset() != state.get().last_scroll_offset {
                                    state.set(state.get().with_last_scroll(
                                        scroll_handle.offset(),
                                        Some(Instant::now()),
                                    ));
                                    cx.notify(view_id);
                                }
                            }
                        }
                    });

                    if is_hover_to_show || is_visible {
                        window.on_mouse_event({
                            let state = scrollbar_state.clone();
                            let scroll_handle = self.scroll_handle.clone();

                            move |event: &MouseDownEvent, phase, _, cx| {
                                if !phase.bubble()
                                    || event.button != MouseButton::Left
                                    || !bounds.contains(&event.position)
                                    || travel <= px(0.)
                                {
                                    return;
                                }
                                cx.stop_propagation();
                                let position = scrollbar_pointer(axis, bounds, event.position);
                                if thumb_bounds.contains(&event.position) {
                                    scroll_handle.start_drag();
                                    let mut current = state.get();
                                    current.drag = Some(ScrollbarDrag {
                                        axis,
                                        grab_offset: position - thumb_start,
                                        last_pointer: position,
                                        offset,
                                        range,
                                        thumb_length,
                                    });
                                    state.set(current);
                                } else {
                                    let offset = scroll_handle.offset();
                                    let percentage = ((position - thumb_length / 2.) / travel).clamp(0., 1.);
                                    let destination = -range * percentage;
                                    scroll_handle.set_offset(if is_vertical {
                                        point(offset.x, destination)
                                    } else {
                                        point(destination, offset.y)
                                    });
                                    scroll_handle.on_scroll(cx);
                                }
                                cx.notify(view_id);
                            }
                        });
                    }

                    window.on_mouse_event({
                        let scroll_handle = self.scroll_handle.clone();
                        let state = scrollbar_state.clone();
                        let min_interval = self.max_fps.map(|fps| Duration::from_secs_f64(1. / fps as f64));

                        move |event: &MouseMoveEvent, phase, _, cx| {
                            if !phase.bubble() {
                                if event.dragging()
                                    && let Some(drag) = state.get().drag.filter(|drag| drag.axis == axis)
                                {
                                    cx.stop_propagation();
                                    if min_interval.is_none_or(|interval| state.get().last_update.elapsed() >= interval)
                                        && move_scrollbar_thumb(&state, &*scroll_handle, drag, bounds, travel, event.position, cx)
                                    {
                                        cx.notify(view_id);
                                    }
                                }
                                return;
                            }
                            let mut notify = false;

                            let need_hover_to_update = is_hover_to_show || is_visible;

                            let hovered = bounds.contains(&event.position) && need_hover_to_update;
                            if hovered != (state.get().hovered_axis == Some(axis)) {
                                state.set(state.get().with_hovered(hovered.then_some(axis)));
                                notify = true;
                            }

                            if thumb_bounds.contains(&event.position) {
                                if state.get().hovered_on_thumb != Some(axis) {
                                    state.set(state.get().with_hovered_on_thumb(Some(axis)));
                                    notify = true;
                                }
                            } else {
                                if state.get().hovered_on_thumb == Some(axis) {
                                    state.set(state.get().with_hovered_on_thumb(None));
                                    notify = true;
                                }
                            }

                            if notify {
                                cx.notify(view_id);
                            }
                        }
                    });

                    window.on_mouse_event({
                        let state = scrollbar_state.clone();
                        let scroll_handle = self.scroll_handle.clone();

                        move |event: &MouseUpEvent, phase, _, cx| {
                            if !phase.bubble()
                                && event.button == MouseButton::Left
                                && let Some(drag) = state.get().drag.filter(|drag| drag.axis == axis)
                            {
                                cx.stop_propagation();
                                move_scrollbar_thumb(&state, &*scroll_handle, drag, bounds, travel, event.position, cx);
                                scroll_handle.end_drag();
                                let mut current = state.get();
                                current.drag = None;
                                state.set(current);
                                cx.notify(view_id);
                            }
                        }
                    });
                }
            },
        );
    }
}
