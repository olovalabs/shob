mod format;
mod inline;
mod node;
mod style;
mod text_view;
mod utils;

use gpui::App;
pub use style::*;
pub use text_view::*;

pub(crate) fn init(cx: &mut App) {
    text_view::init(cx);
}

/// Optional host handler for links in rendered text.
pub struct TextLinkHandler(pub std::rc::Rc<dyn Fn(&str, &mut gpui::Window, &mut gpui::App) -> bool>);
impl gpui::Global for TextLinkHandler {}
