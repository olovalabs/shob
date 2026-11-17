//! TuiMainScreen - Main screen TUI with terminal scrollback.
//!
//! This mode renders content vertically and allows the terminal to handle scrolling.
//! It's suitable for simple output that should be preserved in terminal history.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::layout::composite_tui_line;
use super::overlay::OverlayManager;
use super::tui::{OverlayHandle, OverlayOptions, TuiMode, TuiStopOptions, TUI};
use crate::terminal::Terminal;

/// Main screen TUI that uses terminal scrollback for scrolling.
pub struct TuiMainScreen {
    terminal: Box<dyn Terminal>,
    container: Container,
    running: Mutex<bool>,
    show_hardware_cursor: Mutex<bool>,
    clear_on_shrink: Mutex<bool>,
    focused: Mutex<Option<Arc<dyn Component>>>,
    full_redraw_count: Mutex<usize>,
    overlays: Arc<OverlayManager>,
}

impl TuiMainScreen {
    /// Create a new main screen TUI.
    pub fn new(
        terminal: Box<dyn Terminal>,
        show_hardware_cursor: bool,
        _log_directory: Option<&str>,
    ) -> Self {
        Self {
            terminal,
            container: Container::new(),
            running: Mutex::new(false),
            show_hardware_cursor: Mutex::new(show_hardware_cursor),
            clear_on_shrink: Mutex::new(false),
            focused: Mutex::new(None),
            full_redraw_count: Mutex::new(0),
            overlays: Arc::new(OverlayManager::new()),
        }
    }
}

impl Component for TuiMainScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&self) {
        self.container.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TUI for TuiMainScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Regular
    }

    fn terminal(&self) -> &dyn Terminal {
        self.terminal.as_ref()
    }

    fn children(&self) -> Vec<Arc<dyn Component>> {
        self.container.get_children()
    }

    fn add_child(&self, component: Arc<dyn Component>) {
        self.container.add_child(component);
    }

    fn remove_child(&self, component: &Arc<dyn Component>) {
        self.container.remove_child(component);
    }

    fn clear(&self) {
        self.container.clear();
    }

    fn get_show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
            .lock()
            .map(|s| *s)
            .unwrap_or(false)
    }

    fn set_show_hardware_cursor(&self, enabled: bool) {
        if let Ok(mut show) = self.show_hardware_cursor.lock() {
            *show = enabled;
        }
    }

    fn get_clear_on_shrink(&self) -> bool {
        self.clear_on_shrink.lock().map(|s| *s).unwrap_or(false)
    }

    fn set_clear_on_shrink(&self, enabled: bool) {
        if let Ok(mut clear) = self.clear_on_shrink.lock() {
            *clear = enabled;
        }
    }

    fn set_focus(&self, component: Option<Arc<dyn Component>>) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = component;
        }
    }

    fn get_focus(&self) -> Option<Arc<dyn Component>> {
        self.focused.lock().ok()?.clone()
    }

    fn show_overlay(
        &self,
        component: Arc<dyn Component>,
        options: Option<OverlayOptions>,
    ) -> Arc<dyn OverlayHandle> {
        let handle = self.overlays.add(component, options.unwrap_or_default());
        self.request_render(false);
        Arc::new(ManagedOverlayHandle {
            handle,
            hidden: Mutex::new(false),
            focused: Mutex::new(true),
        })
    }

    fn hide_overlay(&self) {
        self.overlays.remove_topmost();
        self.request_render(false);
    }

    fn has_overlay(&self) -> bool {
        !self.overlays.get_visible().is_empty()
    }

    fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        self.terminal.show_cursor();
        self.request_render(false);
    }

    fn stop(&self, _options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
        self.terminal.show_cursor();
        self.terminal.stop();
    }

    fn render_now(&self, force: bool) {
        if force {
            if let Ok(mut count) = self.full_redraw_count.lock() {
                *count += 1;
            }
        }

        let width = self.terminal.columns();
        let lines = self.render(width);

        // Write the base transcript first. A trailing newline leaves the cursor
        // one row below it, which lets the temporary overlay be painted with
        // relative cursor movement without turning it into scrollback content.
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.terminal.write("\r\n");
            }
            self.terminal.write(line);
        }

        self.paint_overlays(&lines, width);

        self.terminal.flush();
    }

    fn request_render(&self, _force: bool) {
        // Simple immediate render for main screen
        self.render_now(false);
    }

    fn full_redraws(&self) -> usize {
        self.full_redraw_count.lock().map(|c| *c).unwrap_or(0)
    }
}

impl TuiMainScreen {
    fn paint_overlays(&self, base_lines: &[String], width: usize) {
        let height = self.terminal.rows().max(1);
        let overlays = self.overlays.render_visible(width, height);
        if overlays.is_empty() || base_lines.is_empty() {
            return;
        }

        // Move from the cursor at the end of the base output back to its first
        // row, paint each overlay in place, then restore the cursor. The next
        // regular render appends fresh transcript output and naturally replaces
        // the temporary visual layer.
        let base_height = base_lines.len().min(height);
        self.terminal.write("\r\n");
        self.terminal.write(&format!("\x1b[{}A\x1b[s", base_height));

        for overlay in overlays {
            let (x, y, overlay_width) = (overlay.x, overlay.y, overlay.width);
            let overlay_lines = overlay.lines;
            let overlay_height = overlay_lines.len();
            // Restore the first base row before positioning each overlay so
            // multiple overlays retain their z-order and anchor independently.
            self.terminal.write("\x1b[u");
            self.terminal.write(&format!("\x1b[{}B", y));
            for (index, line) in overlay_lines.iter().take(overlay_height).enumerate() {
                if index > 0 {
                    self.terminal.write("\x1b[1B");
                }
                let row = y + index;
                let base_line = base_lines.get(row).map(String::as_str).unwrap_or("");
                let composite = composite_tui_line(base_line, line, x, overlay_width, width);
                self.terminal.write("\r");
                self.terminal.write(&composite);
            }
        }
        // Return the cursor to the append position below the base transcript.
        self.terminal.write("\x1b[u");
        self.terminal.write(&format!("\x1b[{}B", base_height));
    }
}

struct ManagedOverlayHandle {
    handle: super::overlay::OverlayHandle,
    hidden: Mutex<bool>,
    focused: Mutex<bool>,
}

impl OverlayHandle for ManagedOverlayHandle {
    fn hide(&self) {
        self.handle.hide();
        if let Ok(mut hidden) = self.hidden.lock() {
            *hidden = true;
        }
    }
    fn set_hidden(&self, hidden: bool) {
        self.handle.set_visible(!hidden);
        if let Ok(mut current) = self.hidden.lock() {
            *current = hidden;
        }
    }
    fn is_hidden(&self) -> bool {
        self.hidden.lock().map(|hidden| *hidden).unwrap_or(true)
    }
    fn focus(&self) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = true;
        }
    }
    fn is_focused(&self) -> bool {
        self.focused.lock().map(|focused| *focused).unwrap_or(false)
    }
}
