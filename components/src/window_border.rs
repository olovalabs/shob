use gpui::{
    canvas, div, point, prelude::FluentBuilder as _, px, AnyElement, App, CursorStyle, Decorations,
    Edges, HitboxBehavior, Hsla, InteractiveElement as _, IntoElement, MouseButton, ParentElement,
    Pixels, Point, RenderOnce, ResizeEdge, Size, Styled as _, Tiling, Window,
};

use crate::ActiveTheme;

#[cfg(not(target_os = "linux"))]
const SHADOW_SIZE: Pixels = px(0.0);
#[cfg(target_os = "linux")]
const SHADOW_SIZE: Pixels = px(12.0);
const BORDER_SIZE: Pixels = px(1.0);
pub(crate) const BORDER_RADIUS: Pixels = px(0.0);

pub fn window_border() -> WindowBorder {
    WindowBorder::new()
}

#[derive(IntoElement, Default)]
pub struct WindowBorder {
    children: Vec<AnyElement>,
}

impl WindowBorder {
    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }
}

pub fn window_paddings(window: &Window) -> Edges<Pixels> {
    if window.is_maximized() || window.is_fullscreen() {
        return Edges::all(px(0.0));
    }

    match window.window_decorations() {
        Decorations::Server => Edges::all(px(0.0)),
        Decorations::Client { tiling } => {
            let mut paddings = Edges::all(SHADOW_SIZE);
            if tiling.top {
                paddings.top = px(0.0);
            }
            if tiling.bottom {
                paddings.bottom = px(0.0);
            }
            if tiling.left {
                paddings.left = px(0.0);
            }
            if tiling.right {
                paddings.right = px(0.0);
            }
            paddings
        }
    }
}

impl ParentElement for WindowBorder {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for WindowBorder {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let decorations = window.window_decorations();
        let is_maximized = window.is_maximized();
        let is_fullscreen = window.is_fullscreen();
        let is_floating = !is_maximized && !is_fullscreen;

        let client_inset = if is_floating { SHADOW_SIZE } else { px(0.0) };
        window.set_client_inset(client_inset);

        div()
            .id("window-backdrop")
            .bg(gpui::transparent_black())
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling, .. } => {
                    let can_resize = is_floating && !tiling.is_tiled();
                    div.bg(gpui::transparent_black())
                        .when(can_resize, |div| {
                            div.child(
                                canvas(
                                    |bounds, window, _| {
                                        window.insert_hitbox(bounds, HitboxBehavior::Normal)
                                    },
                                    move |bounds, hitbox, window, _| {
                                        let mouse = window.mouse_position();
                                        let size = bounds.size;
                                        let Some(edge) =
                                            resize_edge(mouse, SHADOW_SIZE, size, tiling)
                                        else {
                                            return;
                                        };
                                        window.set_cursor_style(
                                            match edge {
                                                ResizeEdge::Top | ResizeEdge::Bottom => {
                                                    CursorStyle::ResizeUpDown
                                                }
                                                ResizeEdge::Left | ResizeEdge::Right => {
                                                    CursorStyle::ResizeLeftRight
                                                }
                                                ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                                    CursorStyle::ResizeUpLeftDownRight
                                                }
                                                ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                                    CursorStyle::ResizeUpRightDownLeft
                                                }
                                            },
                                            &hitbox,
                                        );
                                    },
                                )
                                .size_full()
                                .absolute(),
                            )
                        })
                        .when(is_floating && !(tiling.top || tiling.right), |div| {
                            div.rounded_tr(BORDER_RADIUS)
                        })
                        .when(is_floating && !(tiling.top || tiling.left), |div| {
                            div.rounded_tl(BORDER_RADIUS)
                        })
                        .when(is_floating && !tiling.top, |div| div.pt(SHADOW_SIZE))
                        .when(is_floating && !tiling.bottom, |div| div.pb(SHADOW_SIZE))
                        .when(is_floating && !tiling.left, |div| div.pl(SHADOW_SIZE))
                        .when(is_floating && !tiling.right, |div| div.pr(SHADOW_SIZE))
                        .when(can_resize, |div| {
                            div.on_mouse_down(MouseButton::Left, move |_, window, _| {
                                let size = window.viewport_size();
                                let pos = window.mouse_position();

                                if let Some(edge) = resize_edge(pos, SHADOW_SIZE, size, tiling) {
                                    window.start_window_resize(edge);
                                }
                            })
                        })
                }
            })
            .size_full()
            .child(
                div()
                    .map(|div| match decorations {
                        Decorations::Server => div,
                        Decorations::Client { tiling } => div
                            .when(is_floating && !(tiling.top || tiling.right), |div| {
                                div.rounded_tr(BORDER_RADIUS)
                            })
                            .when(is_floating && !(tiling.top || tiling.left), |div| {
                                div.rounded_tl(BORDER_RADIUS)
                            })
                            .border_color(cx.theme().window_border)
                            .when(is_floating && !tiling.top, |div| div.border_t(BORDER_SIZE))
                            .when(is_floating && !tiling.bottom, |div| {
                                div.border_b(BORDER_SIZE)
                            })
                            .when(is_floating && !tiling.left, |div| div.border_l(BORDER_SIZE))
                            .when(is_floating && !tiling.right, |div| {
                                div.border_r(BORDER_SIZE)
                            })
                            .when(is_floating && !tiling.is_tiled(), |div| {
                                div.shadow(vec![gpui::BoxShadow {
                                    color: Hsla {
                                        h: 0.,
                                        s: 0.,
                                        l: 0.,
                                        a: 0.3,
                                    },
                                    blur_radius: SHADOW_SIZE / 2.,
                                    spread_radius: px(0.),
                                    offset: point(px(0.0), px(0.0)),
                                    inset: false,
                                }])
                            }),
                    })
                    .on_mouse_move(|_e, _, cx| {
                        cx.stop_propagation();
                    })
                    .bg(gpui::transparent_black())
                    .size_full()
                    .children(self.children),
            )
    }
}

fn resize_edge(
    pos: Point<Pixels>,
    shadow_size: Pixels,
    size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let top = !tiling.top && pos.y < shadow_size;
    let bottom = !tiling.bottom && pos.y > size.height - shadow_size;
    let left = !tiling.left && pos.x < shadow_size;
    let right = !tiling.right && pos.x > size.width - shadow_size;

    match (top, bottom, left, right) {
        (true, false, true, false) => Some(ResizeEdge::TopLeft),
        (true, false, false, true) => Some(ResizeEdge::TopRight),
        (false, true, true, false) => Some(ResizeEdge::BottomLeft),
        (false, true, false, true) => Some(ResizeEdge::BottomRight),
        (true, false, false, false) => Some(ResizeEdge::Top),
        (false, true, false, false) => Some(ResizeEdge::Bottom),
        (false, false, true, false) => Some(ResizeEdge::Left),
        (false, false, false, true) => Some(ResizeEdge::Right),
        _ => None,
    }
}
