use std::rc::Rc;

use crate::{
    h_flex, white, ActiveTheme, Icon, IconName, InteractiveElementExt as _, Sizable as _, StyledExt,
};
use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, App, ClickEvent, Context, Decorations, Hsla,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, Point, Render, RenderOnce, StatefulInteractiveElement as _, StyleRefinement, Styled,
    TitlebarOptions, Window, WindowControlArea,
};
use smallvec::SmallVec;

pub const TITLE_BAR_HEIGHT: Pixels = px(34.);

fn title_bar_height(window: &Window) -> Pixels {
    match PlatformStyle::platform() {
        PlatformStyle::Windows => px(32.),
        _ => (1.75 * window.rem_size()).max(px(34.)),
    }
}

fn title_bar_left_padding() -> Pixels {
    match PlatformStyle::platform() {
        PlatformStyle::Mac => px(78.),
        _ => px(12.),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformStyle {
    Mac,
    Linux,
    Windows,
}

impl PlatformStyle {
    const fn platform() -> Self {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Mac
        }
    }
}

#[derive(IntoElement)]
pub struct TitleBar {
    style: StyleRefinement,
    children: SmallVec<[AnyElement; 1]>,
    on_close_window: Option<Rc<Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>>>,
}

impl TitleBar {
    pub fn new() -> Self {
        Self {
            style: StyleRefinement::default(),
            children: SmallVec::new(),
            on_close_window: None,
        }
    }

    pub fn title_bar_options() -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(9.0), px(9.0))),
        }
    }

    pub fn on_close_window(
        mut self,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_close_window = Some(Rc::new(Box::new(f)));
        self
    }
}

// The Windows control buttons have a fixed width of 35px.
//
// We don't need implementation the click event for the control buttons.

#[derive(IntoElement, Clone)]
enum ControlIcon {
    Minimize,
    Restore,
    Maximize,
    Close {
        on_close_window: Option<Rc<Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>>>,
    },
}

fn caption_font_family() -> &'static str {
    "Segoe Fluent Icons"
}

/// Fallback when "Segoe Fluent Icons" (Win11-only) is absent, i.e. on Win10.
fn caption_font_fallback() -> &'static str {
    "Segoe MDL2 Assets"
}

impl ControlIcon {
    fn minimize() -> Self {
        Self::Minimize
    }

    fn restore() -> Self {
        Self::Restore
    }

    fn maximize() -> Self {
        Self::Maximize
    }

    fn close(on_close_window: Option<Rc<Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>>>) -> Self {
        Self::Close { on_close_window }
    }

    fn id(&self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Restore => "restore",
            Self::Maximize => "maximize",
            Self::Close { .. } => "close",
        }
    }

    fn window_control_area(&self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::Restore | Self::Maximize => WindowControlArea::Max,
            Self::Close { .. } => WindowControlArea::Close,
        }
    }

    fn is_close(&self) -> bool {
        matches!(self, Self::Close { .. })
    }

    /// Segoe caption glyph, exactly like Zed's `WindowsCaptionButton::icon`.
    fn caption_glyph(&self) -> &'static str {
        match self {
            Self::Minimize => "\u{e921}",
            Self::Restore => "\u{e923}",
            Self::Maximize => "\u{e922}",
            Self::Close { .. } => "\u{e8bb}",
        }
    }

    /// SVG icon used on Linux (the Segoe font only exists on Windows).
    fn linux_icon(&self) -> IconName {
        match self {
            Self::Minimize => IconName::WindowMinimize,
            Self::Restore => IconName::WindowRestore,
            Self::Maximize => IconName::WindowMaximize,
            Self::Close { .. } => IconName::WindowClose,
        }
    }

    #[inline]
    fn hover_fg(&self, cx: &App) -> Hsla {
        if self.is_close() {
            cx.theme().danger_foreground
        } else {
            cx.theme().secondary_foreground
        }
    }

    // Zed's Windows close button: solid #E81120 fill, white glyph

    #[inline]
    fn hover_bg(&self, cx: &App) -> Hsla {
        if self.is_close() {
            cx.theme().danger
        } else {
            cx.theme().secondary_hover
        }
    }

    #[inline]
    fn active_bg(&self, cx: &mut App) -> Hsla {
        if self.is_close() {
            cx.theme().danger_active
        } else {
            cx.theme().secondary_active
        }
    }

    fn render_windows_button(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui::Rgba;

        let _ = window;
        let enabled = true;
        let area = self.window_control_area();
        let glyph = self.caption_glyph();
        let is_close = self.is_close();

        let (hover_bg, hover_fg, active_bg, active_fg) = if is_close {
            let red: Hsla = Rgba {
                r: 232.0 / 255.0,
                g: 17.0 / 255.0,
                b: 32.0 / 255.0,
                a: 1.0,
            }
            .into();
            (red, white(), red.opacity(0.8), white().opacity(0.8))
        } else {
            (
                cx.theme().secondary_hover,
                cx.theme().foreground,
                cx.theme().secondary_active,
                cx.theme().foreground,
            )
        };

        let mut caption_font = gpui::font(caption_font_family());
        caption_font.fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
            caption_font_fallback().to_string(),
        ]));
        div()
            .id(self.id())
            .font(caption_font)
            .flex()
            .justify_center()
            .content_center()
            .items_center()
            .occlude()
            .w(px(46.))
            .h_full()
            .flex_shrink_0()
            .text_size(px(10.))
            .when(!enabled, |this| {
                this.text_color(cx.theme().secondary_foreground.opacity(0.4))
            })
            .when(enabled, |this| {
                this.hover(|style| style.bg(hover_bg).text_color(hover_fg))
                    .active(|style| style.bg(active_bg).text_color(active_fg))
            })
            .window_control_area(area)
            .child(glyph)
    }

    fn render_linux_button(self, cx: &mut App) -> impl IntoElement {
        let hover_fg = self.hover_fg(cx);
        let hover_bg = self.hover_bg(cx);
        let active_bg = self.active_bg(cx);
        let icon = self.clone();
        let on_close_window = match &self {
            Self::Close { on_close_window } => on_close_window.clone(),
            _ => None,
        };
        let svg_icon = self.linux_icon();

        div()
            .id(self.id())
            .flex()
            .w(px(34.))
            .h_full()
            .flex_shrink_0()
            .justify_center()
            .content_center()
            .items_center()
            .text_color(cx.theme().foreground)
            .hover(|style| style.bg(hover_bg).text_color(hover_fg))
            .active(|style| style.bg(active_bg).text_color(hover_fg))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                match icon {
                    Self::Minimize => window.minimize_window(),
                    Self::Restore | Self::Maximize => window.zoom_window(),
                    Self::Close { .. } => {
                        if let Some(f) = on_close_window.clone() {
                            f(&ClickEvent::default(), window, cx);
                        } else {
                            window.remove_window();
                        }
                    }
                }
            })
            .child(Icon::new(svg_icon).small())
    }
}

impl RenderOnce for ControlIcon {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        match PlatformStyle::platform() {
            PlatformStyle::Windows => self.render_windows_button(window, cx).into_any_element(),
            PlatformStyle::Linux => self.render_linux_button(cx).into_any_element(),

            PlatformStyle::Mac => div().id(self.id()).into_any_element(),
        }
    }
}

#[derive(IntoElement)]
struct WindowControls {
    on_close_window: Option<Rc<Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>>>,
}

impl RenderOnce for WindowControls {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        match PlatformStyle::platform() {
            PlatformStyle::Mac => return div().id("window-controls"),
            _ => {}
        }
        if window.is_fullscreen() {
            return div().id("window-controls");
        }
        if PlatformStyle::platform() == PlatformStyle::Linux
            && !matches!(window.window_decorations(), Decorations::Client { .. })
        {
            return div().id("window-controls");
        }

        let supported = window.window_controls();
        let show_minimize = match PlatformStyle::platform() {
            PlatformStyle::Linux => supported.minimize,
            _ => true,
        };
        let show_zoom = match PlatformStyle::platform() {
            PlatformStyle::Linux => supported.maximize,
            _ => true,
        };

        match PlatformStyle::platform() {
            PlatformStyle::Windows => div()
                .id("window-controls")
                .font_family(caption_font_family())
                .flex()
                .flex_row()
                .justify_center()
                .content_stretch()
                .flex_shrink_0()
                .max_h(title_bar_height(window))
                .min_h(title_bar_height(window))
                .child(ControlIcon::minimize())
                .child(if window.is_maximized() {
                    ControlIcon::restore()
                } else {
                    ControlIcon::maximize()
                })
                .child(ControlIcon::close(self.on_close_window)),
            _ => h_flex()
                .id("window-controls")
                .items_center()
                .flex_shrink_0()
                .h_full()
                .when(show_minimize, |this| this.child(ControlIcon::minimize()))
                .when(show_zoom, |this| {
                    this.child(if window.is_maximized() {
                        ControlIcon::restore()
                    } else {
                        ControlIcon::maximize()
                    })
                })
                .child(ControlIcon::close(self.on_close_window)),
        }
    }
}

impl Styled for TitleBar {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for TitleBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

struct TitleBarState {
    mouse_down_pos: Option<Point<Pixels>>,
}

impl Render for TitleBarState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl RenderOnce for TitleBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let platform_style = PlatformStyle::platform();
        let is_linux = platform_style == PlatformStyle::Linux;
        let is_macos = platform_style == PlatformStyle::Mac;

        let state = window.use_state(cx, |_, _| TitleBarState {
            mouse_down_pos: None,
        });

        div().flex_shrink_0().child(
            div()
                .id("title-bar")
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(title_bar_height(window))
                .pl(title_bar_left_padding())
                .border_b_1()
                .border_color(cx.theme().title_bar_border)
                .bg(cx.theme().title_bar)
                .refine_style(&self.style)
                .when(is_linux, |this| {
                    this.on_double_click(|_, window, _| window.zoom_window())
                })
                .when(is_macos, |this| {
                    this.on_double_click(|_, window, _| window.titlebar_double_click())
                })
                .on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
                    state.mouse_down_pos = None;
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    window.listener_for(&state, |state, ev: &MouseDownEvent, _, _| {
                        state.mouse_down_pos = Some(ev.position);
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    window.listener_for(&state, |state, _, _, _| {
                        state.mouse_down_pos = None;
                    }),
                )
                .on_mouse_move(window.listener_for(
                    &state,
                    |state, ev: &MouseMoveEvent, window, _| {
                        if ev.pressed_button != Some(MouseButton::Left) {
                            state.mouse_down_pos = None;
                            return;
                        }
                        if let Some(start_pos) = state.mouse_down_pos {
                            let dx = (ev.position.x - start_pos.x).to_f64();
                            let dy = (ev.position.y - start_pos.y).to_f64();
                            // Drag threshold of 8px (8^2 = 64) avoids micro-jitter on clicks
                            // triggering premature window movement or unmaximizing.
                            if dx * dx + dy * dy >= 64.0 {
                                state.mouse_down_pos = None;
                                window.start_window_move();
                            }
                        }
                    },
                ))
                .child(
                    h_flex()
                        .id("bar")
                        .window_control_area(WindowControlArea::Drag)
                        .when(window.is_fullscreen(), |this| this.pl_3())
                        .h_full()
                        .justify_between()
                        .flex_shrink_0()
                        .flex_1()
                        .when(is_linux && is_client_decorated, |this| {
                            this.child(
                                div()
                                    .top_0()
                                    .left_0()
                                    .absolute()
                                    .size_full()
                                    .h_full()
                                    .on_mouse_down(MouseButton::Right, move |ev, window, _| {
                                        window.show_window_menu(ev.position)
                                    }),
                            )
                        })
                        .children(self.children),
                )
                .child(WindowControls {
                    on_close_window: self.on_close_window,
                }),
        )
    }
}
