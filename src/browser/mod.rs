//! The browser tool-panel surface: Chrome-style page tabs with an address
//! bar, backed by a native webview on platforms that provide one.

pub mod address;
mod surface;

pub use surface::BrowserSurface;
