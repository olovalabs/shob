use gpui::{
    App, Context, Div, Entity, MouseButton, Render, SharedString, WeakEntity, Window, div,
    prelude::*, px, svg,
};
use gpui_component::input::{Input, InputState};

use crate::AppState;
use crate::theme::ThemeTokens;
use crate::workspace_state::SavedBrowser;

/// Page tabs per browser pane stay bounded; one webview will exist per tab.
const MAX_BROWSER_TABS: usize = 8;

/// One page tab. The native webview is attached to this state on platforms
/// that have one; the state itself is platform-neutral and persists.
#[derive(Clone, Debug)]
pub struct BrowserTab {
    pub url: String,
    pub title: String,
}

impl BrowserTab {
    fn new_tab() -> Self {
        Self {
            url: String::new(),
            title: String::new(),
        }
    }

    /// Page title, falling back to the URL host, then "New Tab".
    pub fn display_title(&self) -> SharedString {
        if !self.title.is_empty() {
            return self.title.clone().into();
        }
        host_of(&self.url)
            .map(SharedString::from)
            .unwrap_or_else(|| "New Tab".into())
    }
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, a)| a)
        .unwrap_or(authority);
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    (!host.is_empty()).then_some(host)
}

/// One browser tool-panel pane: Chrome-style page tabs, address bar, and the
/// page area a native webview is overlaid on.
pub struct BrowserSurface {
    owner: WeakEntity<AppState>,
    pane_id: usize,
    tabs: Vec<BrowserTab>,
    active: usize,
    address: Entity<InputState>,
}

impl BrowserSurface {
    pub fn new(
        owner: WeakEntity<AppState>,
        pane_id: usize,
        saved: SavedBrowser,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut tabs: Vec<BrowserTab> = saved
            .tabs
            .into_iter()
            .map(|url| BrowserTab {
                url,
                title: String::new(),
            })
            .collect();
        if tabs.is_empty() {
            tabs.push(BrowserTab::new_tab());
        }
        let active = saved.active.min(tabs.len() - 1);
        Self {
            owner,
            pane_id,
            tabs,
            active,
            address: cx
                .new(|cx| InputState::new(window, cx).placeholder("Search or enter address")),
        }
    }

    /// What survives a restart: the page-tab URLs and which one was visible.
    pub fn snapshot(&self) -> SavedBrowser {
        SavedBrowser {
            tabs: self.tabs.iter().map(|tab| tab.url.clone()).collect(),
            active: self.active,
        }
    }

    fn tokens(&self, cx: &App) -> ThemeTokens {
        self.owner.upgrade().unwrap().read(cx).tokens(cx)
    }

    fn render_tab_strip(&self, tokens: &ThemeTokens, cx: &mut Context<Self>) -> Div {
        div()
            .h(px(32.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(6.0))
            .border_b_1()
            .border_color(tokens.border_subtle)
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let selected = index == self.active;
                div()
                    .id(("browser-tab", index))
                    .h(px(24.0))
                    .max_w(px(180.0))
                    .min_w(px(76.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(if selected {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    })
                    .when(selected, |tab| tab.bg(tokens.card_hover))
                    .hover(|tab| tab.bg(tokens.card_hover))
                    .child(
                        svg()
                            .path("icons/browser-color.svg")
                            .size(px(12.0))
                            .flex_shrink_0(),
                    )
                    .child(div().min_w_0().truncate().child(tab.display_title()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if this.active != index {
                                this.active = index;
                                cx.notify();
                            }
                        }),
                    )
            }))
            .child(
                div()
                    .id("browser-tab-add")
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|btn| btn.bg(tokens.card_hover))
                    .child(
                        svg()
                            .path("icons/plus.svg")
                            .size(px(12.0))
                            .text_color(tokens.text_secondary),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            if this.tabs.len() < MAX_BROWSER_TABS {
                                this.tabs.push(BrowserTab::new_tab());
                                this.active = this.tabs.len() - 1;
                                cx.notify();
                            }
                        }),
                    ),
            )
    }

    fn render_toolbar(&self, tokens: &ThemeTokens, _cx: &mut Context<Self>) -> Div {
        div()
            .h(px(38.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .child(nav_button(
                tokens,
                "browser-back",
                "icons/arrow-left.svg",
                false,
            ))
            .child(nav_button(
                tokens,
                "browser-forward",
                "icons/arrow-right.svg",
                false,
            ))
            .child(nav_button(
                tokens,
                "browser-reload",
                "ui_icons/refresh.svg",
                true,
            ))
            .child(
                div()
                    .id("browser-address")
                    .flex_1()
                    .min_w_0()
                    .min_h(px(26.0))
                    .flex()
                    .items_center()
                    .px(px(9.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.card_bg)
                    .child(
                        Input::new(&self.address)
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .cleanable(false)
                            .text_size(px(12.5)),
                    ),
            )
    }
}

fn nav_button(
    tokens: &ThemeTokens,
    id: &'static str,
    icon: &str,
    enabled: bool,
) -> impl IntoElement {
    let button = div()
        .id(id)
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .child(
            svg()
                .path(icon)
                .size(px(13.0))
                .text_color(tokens.text_secondary),
        );
    if enabled {
        button
            .cursor_pointer()
            .hover(|btn| btn.bg(tokens.card_hover))
    } else {
        button.opacity(0.35)
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens(cx);
        #[cfg(any(target_os = "macos", windows))]
        let page = div().flex_1().min_h_0().w_full();
        #[cfg(not(any(target_os = "macos", windows)))]
        let page = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .child(svg().path("icons/browser-color.svg").size(px(34.0)))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(tokens.text_muted)
                    .child("Browser is not supported on this platform"),
            );
        div()
            .id(("browser-surface", self.pane_id))
            .debug_selector(|| "browser-surface".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.pane_bg)
            .child(self.render_tab_strip(&tokens, cx))
            .child(self.render_toolbar(&tokens, cx))
            .child(page)
    }
}
