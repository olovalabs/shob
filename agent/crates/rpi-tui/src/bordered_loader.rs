//! Loader wrapped in [`DynamicBorder`]s — port of `bordered-loader.ts`.
//!
//! A display-only wrapper: the caller owns the [`Loader`] (or
//! [`CancellableLoader`]) and drives its lifecycle; this component merely
//! renders borders around it and an optional cancel hint line. There is no
//! async/signal plumbing (the TS `AbortController`/`signal` has no Rust
//! equivalent in this codebase — cancellation flows through the key loop).

use std::any::Any;
use std::sync::Arc;

use super::component::Component;
use crate::dynamic_border::DynamicBorder;
use crate::keybinding_hints::key_hint;
use crate::loader::{CancellableLoader, Loader};
use crate::spacer::Spacer;
use crate::theme::{theme, Color};

/// Which loader kind the [`BorderedLoader`] wraps.
pub enum BorderedLoaderInner {
    /// A non-cancellable loader.
    Plain(Arc<Loader>),
    /// A cancellable loader.
    Cancellable(Arc<CancellableLoader>),
}

/// A loader framed by top and bottom [`DynamicBorder`]s, with an optional
/// `cancel` hint line (when the inner loader is cancellable and running).
pub struct BorderedLoader {
    inner: BorderedLoaderInner,
    border_color: Option<Color>,
}

impl BorderedLoader {
    /// Wrap a plain (non-cancellable) [`Loader`].
    pub fn plain(loader: Arc<Loader>) -> Self {
        Self {
            inner: BorderedLoaderInner::Plain(loader),
            border_color: None,
        }
    }

    /// Wrap a [`CancellableLoader`].
    pub fn cancellable(loader: Arc<CancellableLoader>) -> Self {
        Self {
            inner: BorderedLoaderInner::Cancellable(loader),
            border_color: None,
        }
    }

    /// Override the border color (default: theme border).
    pub fn with_border_color(mut self, color: Color) -> Self {
        self.border_color = Some(color);
        self
    }

    fn border(&self) -> DynamicBorder {
        match self.border_color {
            Some(c) => DynamicBorder::with_color(c),
            None => DynamicBorder::new(),
        }
    }

    fn is_cancellable_running(&self) -> bool {
        match &self.inner {
            BorderedLoaderInner::Cancellable(l) => l.is_running(),
            _ => false,
        }
    }
}

impl Component for BorderedLoader {
    fn render(&self, width: usize) -> Vec<String> {
        let _ = theme(); // ensure global theme is initialized for hint colors
        let mut lines = Vec::new();

        // Top border
        lines.extend(self.border().render(width));

        // Loader body
        let body = match &self.inner {
            BorderedLoaderInner::Plain(l) => l.render(width),
            BorderedLoaderInner::Cancellable(l) => l.render(width),
        };
        lines.extend(body);

        // Cancel hint (cancellable + running only) — mirrors the TS layout:
        // Spacer + Text(keyHint("tui.select.cancel","cancel"))
        if self.is_cancellable_running() {
            lines.extend(Spacer::new(1).render(width));
            lines.push(key_hint("tui.select.cancel", "cancel"));
        }

        // Bottom border
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.border().render(width));

        lines
    }

    fn invalidate(&self) {
        match &self.inner {
            BorderedLoaderInner::Plain(l) => l.invalidate(),
            BorderedLoaderInner::Cancellable(l) => l.invalidate(),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// (no internal helper module needed — borders are inline DynamicBorders.)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_bordered_loader_renders_borders() {
        let loader = Arc::new(Loader::with_text("Working"));
        loader.start();
        let bl = BorderedLoader::plain(loader);
        let lines = bl.render(40);
        // top border, loader line, spacer, bottom border
        assert!(lines.len() >= 3);
        assert!(lines.first().unwrap().contains('─'));
        assert!(lines.last().unwrap().contains('─'));
    }

    #[test]
    fn test_cancellable_adds_hint_when_running() {
        let loader = Arc::new(CancellableLoader::with_text("Working"));
        loader.start();
        let bl = BorderedLoader::cancellable(loader);
        let lines = bl.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains("cancel"));
    }
}
