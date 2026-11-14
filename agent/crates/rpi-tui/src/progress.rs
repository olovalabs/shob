//! Progress bar component for displaying progress.
//!
//! Provides visual progress indicators for tasks.

use std::sync::Mutex;

use super::component::Component;
use crate::ansi::{fg_256, bg_256, bold, dim};

/// Progress bar style.
#[derive(Debug, Clone, Copy, Default)]
pub enum ProgressBarStyle {
    #[default]
    Default,
    Blocks,
    Dots,
    Lines,
}

/// Progress bar options.
#[derive(Debug, Clone)]
pub struct ProgressBarOptions {
    /// Width of the progress bar in characters.
    pub width: usize,
    /// Style of the progress bar.
    pub style: ProgressBarStyle,
    /// Show percentage text.
    pub show_percent: bool,
    /// Label text.
    pub label: Option<String>,
}

impl Default for ProgressBarOptions {
    fn default() -> Self {
        Self {
            width: 30,
            style: ProgressBarStyle::Default,
            show_percent: true,
            label: None,
        }
    }
}

/// ProgressBar - A visual progress indicator.
pub struct ProgressBar {
    progress: Mutex<f64>,
    options: ProgressBarOptions,
}

impl ProgressBar {
    /// Create a new progress bar.
    pub fn new(options: ProgressBarOptions) -> Self {
        Self {
            progress: Mutex::new(0.0),
            options,
        }
    }

    /// Create a simple progress bar with default options.
    pub fn simple(width: usize) -> Self {
        Self::new(ProgressBarOptions {
            width,
            ..Default::default()
        })
    }

    /// Set the progress (0.0 to 1.0).
    pub fn set_progress(&self, progress: f64) {
        if let Ok(mut p) = self.progress.lock() {
            *p = progress.clamp(0.0, 1.0);
        }
    }

    /// Get the current progress.
    pub fn get_progress(&self) -> f64 {
        self.progress.lock().map(|p| *p).unwrap_or(0.0)
    }

    /// Render the bar portion.
    fn render_bar(&self, width: usize, progress: f64) -> String {
        let filled = (width as f64 * progress).round() as usize;
        let empty = width.saturating_sub(filled);

        match self.options.style {
            ProgressBarStyle::Default | ProgressBarStyle::Blocks => {
                let filled_str: String = "█".repeat(filled);
                let empty_str: String = "░".repeat(empty);
                format!("{}{}{}{}", bg_256(28, &filled_str), bg_256(238, &empty_str), "\x1b[0m", "")
            }
            ProgressBarStyle::Dots => {
                let filled_str: String = "●".repeat(filled);
                let empty_str: String = "○".repeat(empty);
                format!("{}{}", fg_256(28, &filled_str), dim(&empty_str))
            }
            ProgressBarStyle::Lines => {
                let filled_str: String = "━".repeat(filled);
                let empty_str: String = "─".repeat(empty);
                format!("{}{}", fg_256(28, &filled_str), dim(&empty_str))
            }
        }
    }
}

impl Component for ProgressBar {
    fn render(&self, _width: usize) -> Vec<String> {
        let progress = self.get_progress();
        let mut line = String::new();

        // Add label if present
        if let Some(label) = &self.options.label {
            line.push_str(&bold(label));
            line.push(' ');
        }

        // Add progress bar
        line.push_str(&self.render_bar(self.options.width, progress));

        // Add percentage
        if self.options.show_percent {
            let percent = (progress * 100.0).round() as usize;
            line.push_str(&format!(" {:>3}%", percent));
        }

        vec![line]
    }

    fn invalidate(&self) {
        // No cache
    }
}

/// Spinner - An animated loading indicator.
pub struct Spinner {
    state: Mutex<usize>,
    frames: Vec<String>,
    message: Mutex<String>,
}

impl Spinner {
    /// Create a new spinner with default frames.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(0),
            frames: vec![
                "⠋".into(), "⠙".into(), "⠹".into(), "⠸".into(),
                "⠼".into(), "⠴".into(), "⠦".into(), "⠧".into(),
                "⠇".into(), "⠏".into(),
            ],
            message: Mutex::new(String::new()),
        }
    }

    /// Create a spinner with custom frames.
    pub fn with_frames(frames: Vec<String>) -> Self {
        Self {
            state: Mutex::new(0),
            frames,
            message: Mutex::new(String::new()),
        }
    }

    /// Set the message.
    pub fn set_message(&self, message: impl Into<String>) {
        if let Ok(mut m) = self.message.lock() {
            *m = message.into();
        }
    }

    /// Advance to the next frame.
    pub fn tick(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = (*state + 1) % self.frames.len();
        }
    }

    /// Get the current frame.
    fn current_frame(&self) -> String {
        let state = self.state.lock().map(|s| *s).unwrap_or(0);
        self.frames.get(state).cloned().unwrap_or_else(|| "⠋".into())
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Spinner {
    fn render(&self, _width: usize) -> Vec<String> {
        let frame = self.current_frame();
        let message = self.message.lock().map(|m| m.clone()).unwrap_or_default();

        let line = if message.is_empty() {
            format!("{}", fg_256(14, &frame))
        } else {
            format!("{} {}", fg_256(14, &frame), message)
        };

        vec![line]
    }

    fn invalidate(&self) {
        // No cache
    }
}

/// Dots - A simple dots loading animation.
pub struct DotsLoader {
    count: Mutex<usize>,
    max_dots: usize,
}

impl DotsLoader {
    /// Create a new dots loader.
    pub fn new(max_dots: usize) -> Self {
        Self {
            count: Mutex::new(0),
            max_dots,
        }
    }

    /// Advance dots.
    pub fn tick(&self) {
        if let Ok(mut count) = self.count.lock() {
            *count = (*count + 1) % (self.max_dots + 1);
        }
    }
}

impl Component for DotsLoader {
    fn render(&self, _width: usize) -> Vec<String> {
        let count = self.count.lock().map(|c| *c).unwrap_or(0);
        let dots = ".".repeat(count);
        let spaces = " ".repeat(self.max_dots - count);
        
        vec![format!("Loading{}{}  ", dots, spaces)]
    }

    fn invalidate(&self) {
        // No cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar() {
        let bar = ProgressBar::simple(20);
        bar.set_progress(0.5);
        assert_eq!(bar.get_progress(), 0.5);
    }

    #[test]
    fn test_spinner() {
        let spinner = Spinner::new();
        spinner.set_message("Loading...");
        spinner.tick();
        let lines = spinner.render(40);
        assert!(!lines[0].is_empty());
    }
}