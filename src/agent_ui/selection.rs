//! Native selection controls used by agent-generated cards.
use gpui::{AnyElement, App, IntoElement, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::{
    IndexPath,
    select::{SearchableVec, Select, SelectState},
};

use super::{prop_bool, prop_str, prop_strings, prop_usize};

pub(super) fn select(
    id: SharedString,
    props: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let options = prop_strings(props, "options");
    let selected = prop_usize(props, "selected")
        .filter(|index| *index < options.len())
        .map(|index| IndexPath::default().row(index));
    let searchable = prop_bool(props, "searchable").unwrap_or(false);
    let state = window.use_keyed_state(id, cx, |window, cx| {
        SelectState::new(SearchableVec::new(options), selected, window, cx).searchable(searchable)
    });
    let mut container = div().w_full().flex().flex_col().gap(px(4.0));
    if let Some(label) = prop_str(props, "label") {
        container = container.child(div().text_size(px(12.0)).child(label));
    }
    container
        .child(
            Select::new(&state)
                .placeholder(prop_str(props, "placeholder").unwrap_or_else(|| "Select…".into()))
                .disabled(prop_bool(props, "disabled").unwrap_or(false)),
        )
        .into_any_element()
}
