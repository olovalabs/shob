//! Native numeric controls for agent-generated cards.
use gpui::{
    AnyElement, App, AppContext as _, Entity, IntoElement, ParentElement, SharedString, Styled,
    Subscription, Window, div, px,
};
use gpui_component::{
    Disableable as _,
    input::{InputState, NumberInput, NumberInputEvent, StepAction},
    slider::{Slider, SliderState},
};

use super::{prop_bool, prop_f64, prop_str};

#[derive(Clone, Copy)]
struct Range {
    min: f64,
    max: f64,
    step: f64,
    value: f64,
}

impl Range {
    fn from_props(props: &serde_json::Value) -> Self {
        // Bound values to the range representable by GPUI's f32 slider.
        let number = |key, default: f64| {
            prop_f64(props, key)
                .filter(|n| n.is_finite())
                .unwrap_or(default)
                .clamp(-1.0e9, 1.0e9)
        };
        let min = number("min", 0.0);
        let max = number("max", 100.0).max(min + 0.001);
        let step = number("step", 1.0).abs().max(0.001);
        let value = number("value", min).clamp(min, max);
        Self {
            min,
            max,
            step,
            value,
        }
    }

    fn stepped(self, value: f64, increment: bool) -> f64 {
        let value = if value.is_finite() { value } else { self.value };
        (value + if increment { self.step } else { -self.step }).clamp(self.min, self.max)
    }
}

struct NumberState {
    input: Entity<InputState>,
    _subscription: Subscription,
}

pub(super) fn number_input(
    id: SharedString,
    props: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let range = Range::from_props(props);
    let state = window.use_keyed_state(id, cx, |window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(range.value.to_string()));
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |_: &mut NumberState, input, event: &NumberInputEvent, window, cx| {
                let NumberInputEvent::Step(action) = event;
                let value = input.read(cx).value().parse::<f64>().unwrap_or(range.value);
                let next = range.stepped(value, *action == StepAction::Increment);
                input.update(cx, |input, cx| {
                    input.set_value(next.to_string(), window, cx)
                });
            },
        );
        NumberState {
            input,
            _subscription: subscription,
        }
    });
    let input = state.read(cx).input.clone();
    let mut container = div().w_full().flex().flex_col().gap(px(4.0));
    if let Some(label) = prop_str(props, "label") {
        container = container.child(div().text_size(px(12.0)).child(label));
    }
    container
        .child(NumberInput::new(&input).disabled(prop_bool(props, "disabled").unwrap_or(false)))
        .into_any_element()
}

pub(super) fn slider(
    id: SharedString,
    props: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let range = Range::from_props(props);
    let state = window.use_keyed_state(id, cx, |_, _| {
        let min = range.min as f32;
        let max = (range.max as f32).max(min + min.abs().max(1.0) * f32::EPSILON * 2.0);
        SliderState::new()
            .min(min)
            .max(max)
            .step(range.step as f32)
            .default_value(range.value as f32)
    });
    let mut container = div().w_full().flex().flex_col().gap(px(8.0));
    if let Some(label) = prop_str(props, "label") {
        container = container.child(div().text_size(px(12.0)).child(label));
    }
    container
        .child(
            Slider::new(&state)
                .disabled(prop_bool(props, "disabled").unwrap_or(false))
                .w_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_bounds_and_step_are_safe() {
        let range = Range::from_props(&serde_json::json!({"min":2,"max":5,"step":2,"value":99}));
        assert_eq!(range.value, 5.0);
        assert_eq!(range.stepped(4.0, true), 5.0);
        assert_eq!(range.stepped(3.0, false), 2.0);
        assert!(Range::from_props(&serde_json::json!({"min":3,"max":1,"step":0})).step > 0.0);
    }
}
