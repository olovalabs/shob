//! Rendering utilities for TUI.

use std::cell::Cell;

use crate::ansi::visible_width;

thread_local! {
    static MAIN_SCREEN_FRAME: Cell<bool> = const { Cell::new(false) };
}

/// Render transcript panels without wall-clock-only changes. Saved main-buffer
/// rows cannot be patched; live elapsed belongs in the editor/status dock.
/// The scope is thread-local and restored even if a nested render panics.
pub(crate) fn with_main_screen_frame<R>(render: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            MAIN_SCREEN_FRAME.with(|frame| frame.set(self.0));
        }
    }
    let _restore = Restore(MAIN_SCREEN_FRAME.with(|frame| frame.replace(true)));
    render()
}

pub(crate) fn is_main_screen_frame() -> bool {
    MAIN_SCREEN_FRAME.with(Cell::get)
}

/// Render state for tracking previous output.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    pub lines: Vec<String>,
    pub width: usize,
    pub height: usize,
}

/// Render a component and composite into a screen buffer.
pub fn render_to_buffer(
    lines: &[String],
    _width: usize,
    height: usize,
    scroll_top: usize,
) -> Vec<String> {
    let visible: Vec<String> = lines
        .iter()
        .skip(scroll_top)
        .take(height)
        .cloned()
        .collect();

    // Pad if needed
    let mut visible = visible;
    while visible.len() < height {
        visible.push(String::new());
    }

    visible
}

/// Apply line resets to ensure styles don't leak between lines.
pub fn apply_line_resets(lines: Vec<String>) -> Vec<String> {
    const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

    lines
        .into_iter()
        .map(|line| {
            if line.contains("\x1b[") {
                format!("{}{}", line, SEGMENT_RESET)
            } else {
                line
            }
        })
        .collect()
}

/// Composite a line from overlay onto a base line.
///
/// # Arguments
/// * `base` - The base line to composite onto
/// * `overlay` - The overlay line to composite
/// * `col` - The column position to start the overlay
/// * `width` - The maximum width of the result line (for bounds checking)
///
/// # Returns
/// A new string with the overlay composited onto the base
pub fn composite_line(base: &str, overlay: &str, col: usize, width: usize) -> String {
    let overlay_width = visible_width(overlay);

    if col >= width || overlay_width == 0 {
        return base.to_string();
    }

    // For a simple implementation without ANSI handling:
    let base_chars: Vec<char> = base.chars().collect();

    // Build the result
    let mut result = String::new();

    // Add characters before the overlay position
    for i in 0..col {
        if i < base_chars.len() {
            result.push(base_chars[i]);
        } else {
            result.push(' ');
        }
    }

    // Add the overlay
    result.push_str(overlay);

    // Add characters after the overlay, skipping the replaced region
    let after_start = col + overlay_width;
    for i in after_start..base_chars.len() {
        result.push(base_chars[i]);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_screen_scope_is_nested_thread_local_and_unwind_safe() {
        assert!(!is_main_screen_frame());
        with_main_screen_frame(|| {
            assert!(is_main_screen_frame());
            with_main_screen_frame(|| assert!(is_main_screen_frame()));
            assert!(is_main_screen_frame());
            std::thread::spawn(|| assert!(!is_main_screen_frame()))
                .join()
                .unwrap();
            let result = std::panic::catch_unwind(|| {
                with_main_screen_frame(|| panic!("render failed"));
            });
            assert!(result.is_err());
            assert!(is_main_screen_frame());
        });
        assert!(!is_main_screen_frame());
    }

    #[test]
    fn test_composite_line() {
        let base = "Hello, World!";
        let overlay = "Rust";
        let result = composite_line(base, overlay, 7, 20);
        // Overlay replaces "Worl" at positions 7-10, preserving "d!" after
        assert_eq!(result, "Hello, Rustd!");
    }

    #[test]
    fn test_composite_line_short_base() {
        let base = "Hi";
        let overlay = "Hello";
        let result = composite_line(base, overlay, 0, 10);
        // Overlay replaces all of "Hi" plus extends beyond
        assert_eq!(result, "Hello");
    }
}
