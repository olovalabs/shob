use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, PromptButton, PromptLevel, PromptResponse,
    Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window,
    div, px,
};

use crate::{AppState, theme::ThemeTokens};

pub(crate) fn install(state: WeakEntity<AppState>, cx: &mut App) {
    cx.set_prompt_builder(move |level, title, detail, actions, handle, window, cx| {
        let prompt = cx.new(|cx| ConfirmationPrompt {
            state: state.clone(),
            level,
            title: title.to_owned().into(),
            detail: detail.map(|text| text.to_owned().into()),
            buttons: actions
                .iter()
                .cloned()
                .map(|button| (button, cx.focus_handle()))
                .collect(),
            cancel: actions.iter().position(PromptButton::is_cancel),
            focus: cx.focus_handle(),
        });
        handle.with_view(prompt, window, cx)
    });
}

struct ConfirmationPrompt {
    state: WeakEntity<AppState>,
    level: PromptLevel,
    title: SharedString,
    detail: Option<SharedString>,
    buttons: Vec<(PromptButton, FocusHandle)>,
    cancel: Option<usize>,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for ConfirmationPrompt {}

impl Focusable for ConfirmationPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.cancel
            .map_or(&self.focus, |index| &self.buttons[index].1)
            .clone()
    }
}

impl ConfirmationPrompt {
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                if let Some(index) = self.cancel {
                    cx.emit(PromptResponse(index));
                }
            }
            "tab" if !self.buttons.is_empty() => {
                let len = self.buttons.len();
                let current = self
                    .buttons
                    .iter()
                    .position(|(_, focus)| focus.is_focused(window));
                let next = if event.keystroke.modifiers.shift {
                    current.map_or(len - 1, |index| (index + len - 1) % len)
                } else {
                    current.map_or(0, |index| (index + 1) % len)
                };
                window.focus(&self.buttons[next].1, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for ConfirmationPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.state.upgrade().map_or_else(
            || {
                ThemeTokens::from_mode_with_scheme(
                    gpui_component::Theme::global(cx).mode,
                    Default::default(),
                )
            },
            |state| state.read(cx).tokens(cx),
        );
        let buttons = self
            .buttons
            .iter()
            .enumerate()
            .map(|(index, (button, focus))| {
                let destructive =
                    matches!(button, PromptButton::Ok(_)) && self.level != PromptLevel::Info;
                let (bg, hover, fg) = if destructive {
                    (
                        tokens.card_bg.blend(tokens.block_failure.opacity(0.12)),
                        tokens.card_bg.blend(tokens.block_failure.opacity(0.22)),
                        tokens.block_failure,
                    )
                } else {
                    (
                        tokens.secondary_btn_bg,
                        tokens.secondary_btn_hover,
                        tokens.secondary_btn_fg,
                    )
                };
                div()
                    .id(index)
                    .role(Role::Button)
                    .track_focus(focus)
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(bg)
                    .text_sm()
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .focus_visible(move |style| style.border_color(tokens.ring))
                    .child(button.label().clone())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PromptResponse(index));
                        cx.stop_propagation();
                    }))
            });
        div()
            .size_full()
            .occlude()
            .p(px(16.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(tokens.window_bg.opacity(0.68))
            .child(
                div()
                    .id("confirmation-prompt")
                    .role(Role::Dialog)
                    .aria_label(self.title.clone())
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .w(px(400.0))
                    .max_w_full()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .p(px(20.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .text_color(tokens.text_primary)
                    .whitespace_normal()
                    .shadow_lg()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(self.title.clone()),
                    )
                    .children(self.detail.clone().map(|detail| {
                        div()
                            .text_sm()
                            .text_color(tokens.text_secondary)
                            .child(detail)
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .gap(px(8.0))
                            .pt(px(8.0))
                            .children(buttons),
                    ),
            )
    }
}
