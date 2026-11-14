use std::{ops::Deref, sync::Arc};

use gpui::{
    App, AppContext, Axis, Context, Element, Empty, Entity, IntoElement, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, Style, StyleRefinement, Styled as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use serde::{Deserialize, Serialize};

use crate::{
    StyledExt,
    resizable::{PANEL_MIN_SIZE, resize_handle},
};

use super::{DockArea, DockItem, PanelView, TabPanel};

#[derive(Clone)]
struct ResizePanel;

impl Render for ResizePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DockPlacement {
    #[serde(rename = "center")]
    Center,
    #[serde(rename = "left")]
    Left,
    #[serde(rename = "bottom")]
    Bottom,
    #[serde(rename = "right")]
    Right,
}

impl DockPlacement {
    fn axis(&self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Bottom => Axis::Vertical,
            Self::Center => unreachable!(),
        }
    }

    pub fn is_left(&self) -> bool {
        matches!(self, Self::Left)
    }

    pub fn is_bottom(&self) -> bool {
        matches!(self, Self::Bottom)
    }

    pub fn is_right(&self) -> bool {
        matches!(self, Self::Right)
    }
}

pub struct Dock {
    pub(super) placement: DockPlacement,
    dock_area: WeakEntity<DockArea>,
    pub(crate) panel: DockItem,

    pub(super) size: Pixels,
    pub(super) open: bool,

    pub(super) collapsible: bool,

    resizing: bool,
}

impl Dock {
    pub(crate) fn new(
        dock_area: WeakEntity<DockArea>,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| {
            let mut tab = TabPanel::new(None, dock_area.clone(), window, cx);
            tab.closable = false;
            tab
        });

        let panel = DockItem::Tabs {
            size: None,
            items: Vec::new(),
            active_ix: 0,
            view: panel.clone(),
        };

        Self::subscribe_panel_events(dock_area.clone(), &panel, window, cx);

        Self {
            placement,
            dock_area,
            panel,
            open: true,
            collapsible: true,
            size: px(200.0),
            resizing: false,
        }
    }

    pub fn left(
        dock_area: WeakEntity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(dock_area, DockPlacement::Left, window, cx)
    }

    pub fn bottom(
        dock_area: WeakEntity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(dock_area, DockPlacement::Bottom, window, cx)
    }

    pub fn right(
        dock_area: WeakEntity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(dock_area, DockPlacement::Right, window, cx)
    }

    pub fn set_collapsible(&mut self, collapsible: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsible = collapsible;
        if !collapsible {
            self.open = true
        }
        cx.notify();
    }

    pub(super) fn from_state(
        dock_area: WeakEntity<DockArea>,
        placement: DockPlacement,
        size: Pixels,
        panel: DockItem,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::subscribe_panel_events(dock_area.clone(), &panel, window, cx);

        if !open {
            match panel.clone() {
                DockItem::Tabs { view, .. } => {
                    view.update(cx, |panel, cx| {
                        panel.set_collapsed(true, window, cx);
                    });
                }
                DockItem::Split { items, .. } => {
                    for item in items {
                        item.set_collapsed(true, window, cx);
                    }
                }
                _ => {}
            }
        }

        Self {
            placement,
            dock_area,
            panel,
            open,
            size,
            collapsible: true,
            resizing: false,
        }
    }

    fn subscribe_panel_events(
        dock_area: WeakEntity<DockArea>,
        panel: &DockItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match panel {
            DockItem::Tabs { view, .. } => {
                window.defer(cx, {
                    let view = view.clone();
                    move |window, cx| {
                        _ = dock_area.update(cx, |this, cx| {
                            this.subscribe_panel(&view, window, cx);
                        });
                    }
                });
            }
            DockItem::Split { items, view, .. } => {
                for item in items {
                    Self::subscribe_panel_events(dock_area.clone(), item, window, cx);
                }
                window.defer(cx, {
                    let view = view.clone();
                    move |window, cx| {
                        _ = dock_area.update(cx, |this, cx| {
                            this.subscribe_panel(&view, window, cx);
                        });
                    }
                });
            }
            DockItem::Tiles { view, .. } => {
                window.defer(cx, {
                    let view = view.clone();
                    move |window, cx| {
                        _ = dock_area.update(cx, |this, cx| {
                            this.subscribe_panel(&view, window, cx);
                        });
                    }
                });
            }
            DockItem::Panel { .. } => {

            }
        }
    }

    pub fn set_panel(&mut self, panel: DockItem, _: &mut Window, cx: &mut Context<Self>) {
        self.panel = panel;
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_open(!self.open, window, cx);
    }

    pub fn size(&self) -> Pixels {
        self.size
    }

    pub fn set_size(&mut self, size: Pixels, _: &mut Window, cx: &mut Context<Self>) {
        self.size = size.max(PANEL_MIN_SIZE);
        cx.notify();
    }

    pub fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open = open;
        let item = self.panel.clone();
        cx.defer_in(window, move |_, window, cx| {
            item.set_collapsed(!open, window, cx);
        });
        cx.notify();
    }

    pub fn add_panel(
        &mut self,
        panel: Arc<dyn PanelView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel
            .add_panel(panel, &self.dock_area, None, window, cx);
        cx.notify();
    }

    pub fn remove_panel(
        &mut self,
        panel: Arc<dyn PanelView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel.remove_panel(panel, window, cx);
        cx.notify();
    }

    fn render_resize_handle(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let axis = self.placement.axis();
        let view = cx.entity().clone();

        resize_handle("resize-handle", axis)
            .placement(self.placement)
            .on_drag(ResizePanel {}, move |info, _, _, cx| {
                cx.stop_propagation();
                view.update(cx, |view, _| {
                    view.resizing = true;
                });
                cx.new(|_| info.deref().clone())
            })
    }
    fn resize(&mut self, mouse_position: Point<Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        if !self.resizing {
            return;
        }

        let dock_area = self
            .dock_area
            .upgrade()
            .expect("DockArea is missing")
            .read(cx);
        let area_bounds = dock_area.bounds;
        let mut left_dock_size = px(0.0);
        let mut right_dock_size = px(0.0);

        if let Some(left_dock) = &dock_area.left_dock {
            if left_dock.entity_id() != cx.entity().entity_id() {
                let left_dock_read = left_dock.read(cx);
                if left_dock_read.is_open() {
                    left_dock_size = left_dock_read.size;
                }
            }
        }

        if let Some(right_dock) = &dock_area.right_dock {
            if right_dock.entity_id() != cx.entity().entity_id() {
                let right_dock_read = right_dock.read(cx);
                if right_dock_read.is_open() {
                    right_dock_size = right_dock_read.size;
                }
            }
        }

        let size = match self.placement {
            DockPlacement::Left => mouse_position.x - area_bounds.left(),
            DockPlacement::Right => area_bounds.right() - mouse_position.x,
            DockPlacement::Bottom => area_bounds.bottom() - mouse_position.y,
            DockPlacement::Center => unreachable!(),
        };
        match self.placement {
            DockPlacement::Left => {
                let max_size = area_bounds.size.width - PANEL_MIN_SIZE - right_dock_size;
                self.size = size.clamp(PANEL_MIN_SIZE, max_size);
            }
            DockPlacement::Right => {
                let max_size = area_bounds.size.width - PANEL_MIN_SIZE - left_dock_size;
                self.size = size.clamp(PANEL_MIN_SIZE, max_size);
            }
            DockPlacement::Bottom => {
                let max_size = area_bounds.size.height - PANEL_MIN_SIZE;
                self.size = size.clamp(PANEL_MIN_SIZE, max_size);
            }
            DockPlacement::Center => unreachable!(),
        }

        cx.notify();
    }

    fn done_resizing(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.resizing = false;
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        if !self.open && !self.placement.is_bottom() {
            return div();
        }

        let cache_style = StyleRefinement::default().absolute().size_full();

        div()
            .relative()
            .overflow_hidden()
            .map(|this| match self.placement {
                DockPlacement::Left | DockPlacement::Right => this.h_flex().h_full().w(self.size),
                DockPlacement::Bottom => this.w_full().h(self.size),
                DockPlacement::Center => unreachable!(),
            })

            .when(!self.open && self.placement.is_bottom(), |this| {
                this.h(px(29.))
            })
            .map(|this| match &self.panel {
                DockItem::Split { view, .. } => this.child(view.clone()),
                DockItem::Tabs { view, .. } => this.child(view.clone()),
                DockItem::Panel { view, .. } => this.child(view.clone().view().cached(cache_style)),

                DockItem::Tiles { .. } => this,
            })
            .child(self.render_resize_handle(window, cx))
            .child(DockElement {
                view: cx.entity().clone(),
            })
    }
}

struct DockElement {
    view: Entity<Dock>,
}

impl IntoElement for DockElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for DockElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        (window.request_layout(Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _window: &mut gpui::Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        ()
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        window.on_mouse_event({
            let view = self.view.clone();
            let resizing = view.read(cx).resizing;
            move |e: &MouseMoveEvent, phase, window, cx| {
                if !resizing {
                    return;
                }
                if !phase.bubble() {
                    return;
                }

                view.update(cx, |view, cx| view.resize(e.position, window, cx))
            }
        });

        window.on_mouse_event({
            let view = self.view.clone();
            move |_: &MouseUpEvent, phase, window, cx| {
                if phase.bubble() {
                    view.update(cx, |view, cx| view.done_resizing(window, cx));
                }
            }
        })
    }
}
