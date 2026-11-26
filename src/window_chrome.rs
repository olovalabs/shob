use gpui::{Pixels, Window, WindowOptions, px};

pub(crate) fn window_options() -> WindowOptions {
    let options = gpui_kit::component::TitleBar::window_options();
    #[cfg(target_os = "macos")]
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: None,
        }),
        app_owns_titlebar_drag: false,
        ..options
    };
    options
}

#[cfg(target_os = "macos")]
pub(crate) fn titlebar_inset(window: &Window) -> Pixels {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return px(0.0);
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return px(0.0);
    };
    // SAFETY: GPUI lends its live NSView for the window borrow, on the main thread.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(native_window) = view.window() else {
        return px(0.0);
    };
    let view_frame = view.convertRect_toView(view.bounds(), None);
    let content = native_window.contentLayoutRect();
    px((view_frame.max().y - content.max().y).max(0.0) as f32)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn titlebar_inset(_: &Window) -> Pixels {
    px(0.0)
}
