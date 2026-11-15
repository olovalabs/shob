#[cfg(target_os = "linux")]
mod linux;

use gpui::{App, Entity, Window};
use gpui_component::WindowControls;

pub(crate) fn load<T: 'static>(_window: &mut Window, _view: &Entity<T>, _cx: &mut App) {
    #[cfg(target_os = "linux")]
    _view.update(_cx, |_, cx| linux::load(_window, cx));
}

pub(crate) fn render(_cx: &App, _dark: bool, leading: bool) -> Option<WindowControls> {
    #[cfg(target_os = "linux")]
    if let Some(controls) = linux::controls(_cx, _dark, leading) {
        return Some(controls);
    }
    (!leading).then(WindowControls::new)
}
