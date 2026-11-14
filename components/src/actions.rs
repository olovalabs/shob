use gpui::{actions, Action};
use serde::Deserialize;

#[derive(Clone, Action, PartialEq, Eq, Deserialize)]
#[action(namespace = kui, no_json)]
pub struct Confirm {

    pub secondary: bool,
}

actions!(kui, [Cancel, SelectUp, SelectDown, SelectLeft, SelectRight]);
