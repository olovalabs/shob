# Window controls

`mod.rs` is the platform entry point for the workspace and Settings title bars.
macOS uses AppKit's traffic lights through GPUI. Windows uses the caption buttons
in `components/src/title_bar.rs`. Add platform-specific settings or style
adapters beside `linux/` and call them from `mod.rs`.

On Linux, `linux/mod.rs` reads the desktop's button layout and loads symbolic
icons from the active icon theme. To add a custom appearance:

1. Add a module in `linux/` next to `plasma_breeze.rs`.
2. Register its `STYLE` in `linux/styles.rs`. The registry matches the desktop
   and icon theme prefix, taking the first match. Put more specific matches first.
3. Verify minimize, maximize, restore and close in both light and dark appearance.
   The icon slots use that order.

A style supplies hover and pressed appearances. GPUI handles drawing, tooltips
and clicks. Desktops and themes without a matching style use the shared Linux
controls with their system icons and button layout.
