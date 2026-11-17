//! Base TUI trait and types.
//!
//! Defines the core TUI interface that both main-screen and alt-screen modes implement.

use std::sync::Arc;

use super::component::Component;
use crate::terminal::Terminal;

/// TUI mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    /// Regular mode with terminal scrollback.
    Regular,
    /// Fullscreen mode using alternate screen buffer.
    Fullscreen,
}

/// Anchor position for overlays.
#[derive(Debug, Clone, Copy, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// Margin configuration for overlays.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

/// Value that can be absolute (number) or percentage (string like "50%").
#[derive(Debug, Clone, Copy)]
pub enum SizeValue {
    Absolute(usize),
    Percent(f64),
}

impl Default for SizeValue {
    fn default() -> Self {
        SizeValue::Absolute(0)
    }
}

/// Options for overlay positioning and sizing.
#[derive(Debug, Clone, Default)]
pub struct OverlayOptions {
    /// Width in columns, or percentage of terminal width.
    pub width: Option<SizeValue>,
    /// Minimum width in columns.
    pub min_width: Option<usize>,
    /// Maximum height in rows, or percentage of terminal height.
    pub max_height: Option<SizeValue>,
    /// Anchor point for positioning.
    pub anchor: OverlayAnchor,
    /// Horizontal offset from anchor position.
    pub offset_x: i32,
    /// Vertical offset from anchor position.
    pub offset_y: i32,
    /// Margin from terminal edges.
    pub margin: Option<OverlayMargin>,
    /// Visibility check function.
    pub visible: Option<fn(width: usize, height: usize) -> bool>,
    /// Don't capture keyboard focus.
    pub non_capturing: bool,
}

/// Handle returned by show_overlay for controlling the overlay.
pub trait OverlayHandle: Send + Sync {
    /// Permanently remove the overlay.
    fn hide(&self);

    /// Temporarily hide or show the overlay.
    fn set_hidden(&self, hidden: bool);

    /// Check if overlay is temporarily hidden.
    fn is_hidden(&self) -> bool;

    /// Focus this overlay.
    fn focus(&self);

    /// Check if this overlay has focus.
    fn is_focused(&self) -> bool;
}

/// Options for stopping the TUI.
#[derive(Debug, Clone, Default)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over.
    pub preserve_screen: bool,
}

/// TUI - Main trait for managing terminal UI with differential rendering.
pub trait TUI: Component + Send + Sync {
    /// Get the TUI mode.
    fn mode(&self) -> TuiMode;

    /// Get the terminal.
    fn terminal(&self) -> &dyn Terminal;

    /// Get the children.
    fn children(&self) -> Vec<Arc<dyn Component>>;

    /// Add a child component.
    fn add_child(&self, component: Arc<dyn Component>);

    /// Remove a child component.
    fn remove_child(&self, component: &Arc<dyn Component>);

    /// Clear all children.
    fn clear(&self);

    /// Get whether to show hardware cursor.
    fn get_show_hardware_cursor(&self) -> bool;

    /// Set whether to show hardware cursor.
    fn set_show_hardware_cursor(&self, enabled: bool);

    /// Get whether to clear on shrink.
    fn get_clear_on_shrink(&self) -> bool;

    /// Set whether to clear on shrink.
    fn set_clear_on_shrink(&self, enabled: bool);

    /// Set the focused component.
    fn set_focus(&self, component: Option<Arc<dyn Component>>);

    /// Get the focused component.
    fn get_focus(&self) -> Option<Arc<dyn Component>>;

    /// Show an overlay component.
    fn show_overlay(
        &self,
        component: Arc<dyn Component>,
        options: Option<OverlayOptions>,
    ) -> Arc<dyn OverlayHandle>;

    /// Hide the topmost overlay.
    fn hide_overlay(&self);

    /// Check if there are any overlays.
    fn has_overlay(&self) -> bool;

    /// Start the TUI event loop.
    fn start(&self);

    /// Stop the TUI.
    fn stop(&self, options: TuiStopOptions);

    /// Render immediately.
    fn render_now(&self, force: bool);

    /// Request a render.
    fn request_render(&self, force: bool);

    /// Get the number of full redraws performed.
    fn full_redraws(&self) -> usize;
}
