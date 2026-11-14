//! User message component — port of `user-message.ts`.
//!
//! Renders the user's submitted text as markdown inside a padded [`Box`] with
//! a subtle background, wrapped in [OSC 133] prompt zone markers so capable
//! terminals can detect user/assistant boundaries. Where OSC 133 is
//! unsupported the sequences are harmless no-ops.
//!
//! Replaces the plain `> text` echo in the transcript.
//!
//! [OSC 133]: https://terminalguide.namepad.de/seq/osc-133/

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::{Component, RenderedDocument};
use crate::box_component::Box;
use crate::markdown::Markdown;
use crate::theme::{theme, theme_revision};

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";
const OSC133_ZONE_FINAL: &str = "\x1b]133;C\x07";

/// Component that renders a user message with a background and OSC133 markers.
pub struct UserMessageComponent {
    text: String,
    output_pad: usize,
    /// Prebuilt inner box (background `Box` + `Markdown`).
    ///
    /// Upstream builds this **once** in the constructor (`user-message.ts`
    /// `rebuild()`); rpi used to rebuild the `Box` + `Markdown` on every
    /// `render`, so the inner caches could never hit and each frame re-parsed
    /// every user message. Rebuild only on `invalidate` / `with_pad` / a theme
    /// change (the background colour is baked into the box).
    inner: Mutex<UserMessageState>,
}

struct UserMessageState {
    bubble: Box,
    built_revision: u64,
    cache: Option<(usize, Arc<Vec<String>>)>,
}

fn build_box(text: &str, output_pad: usize) -> Box {
    let bg = theme().colors.surface;
    let content_box = Box::new(output_pad, 1);
    content_box.set_bg_fn(Some(std::sync::Arc::new(move |s: &str| bg.bg(s))));
    let md = Markdown::new(text.to_string(), 0, 0);
    content_box.add_child(std::sync::Arc::new(md));
    content_box
}

impl UserMessageComponent {
    /// Create a new user message component.
    ///
    /// `output_pad` is the horizontal+vertical padding inside the background
    /// box (mirrors the TS `outputPad`, default 1).
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let inner = build_box(&text, 1);
        Self {
            text,
            output_pad: 1,
            inner: Mutex::new(UserMessageState {
                bubble: inner,
                built_revision: theme_revision(),
                cache: None,
            }),
        }
    }

    /// Override the inner padding.
    pub fn with_pad(self, pad: usize) -> Self {
        let inner = build_box(&self.text, pad);
        Self {
            text: self.text,
            output_pad: pad,
            inner: Mutex::new(UserMessageState {
                bubble: inner,
                built_revision: theme_revision(),
                cache: None,
            }),
        }
    }
}

impl UserMessageComponent {
    fn shared_lines(&self, width: usize) -> Arc<Vec<String>> {
        // Box rebuilding and decorated-row publication share one lock, so an
        // invalidation can never leave rows from the previous bubble cached.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let revision = theme_revision();
        if inner.built_revision != revision {
            inner.bubble = build_box(&self.text, self.output_pad);
            inner.built_revision = revision;
            inner.cache = None;
        }
        if let Some((cached_width, lines)) = inner.cache.as_ref() {
            if *cached_width == width {
                return Arc::clone(lines);
            }
        }

        let mut lines = inner.bubble.render(width);
        if lines.is_empty() {
            let lines = Arc::new(lines);
            inner.cache = Some((width, Arc::clone(&lines)));
            return lines;
        }
        // Prepend zone-start to the first line, append zone-end+final to the
        // last line. These are zero-width escapes so visible layout is unchanged.
        let first = lines.first_mut().unwrap();
        first.insert_str(0, OSC133_ZONE_START);
        let last = lines.last_mut().unwrap();
        last.insert_str(0, OSC133_ZONE_END);
        last.insert_str(OSC133_ZONE_END.len(), OSC133_ZONE_FINAL);
        let lines = Arc::new(lines);
        inner.cache = Some((width, Arc::clone(&lines)));
        lines
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.shared_lines(width).as_ref().clone()
    }

    fn render_document(&self, width: usize) -> RenderedDocument {
        RenderedDocument::from_shared(self.shared_lines(width))
    }

    fn invalidate(&self) {
        // Clear decorated rows atomically with rebuilding the themed bubble.
        if let Ok(mut inner) = self.inner.lock() {
            inner.bubble = build_box(&self.text, self.output_pad);
            inner.built_revision = theme_revision();
            inner.cache = None;
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    #[test]
    fn shared_rows_reuse_decorated_bubble_and_regenerate() {
        let c = UserMessageComponent::new("abcdefghij");
        let first = c.shared_lines(40);
        let _document = c.render_document(40);
        assert!(Arc::ptr_eq(&first, &c.shared_lines(40)));
        assert_eq!(first.as_ref(), &c.render(40));
        let narrow = c.shared_lines(8);
        assert!(!Arc::ptr_eq(&first, &narrow));
        c.invalidate();
        let refreshed = c.shared_lines(8);
        assert!(!Arc::ptr_eq(&narrow, &refreshed));
        assert_eq!(narrow.as_ref(), refreshed.as_ref());
        for rows in [&first, &narrow, &refreshed] {
            let joined = rows.join("\n");
            assert_eq!(joined.matches(OSC133_ZONE_START).count(), 1);
            assert_eq!(joined.matches(OSC133_ZONE_END).count(), 1);
            assert_eq!(joined.matches(OSC133_ZONE_FINAL).count(), 1);
            let compact = strip_ansi(&joined).replace([' ', '\n'], "");
            assert_eq!(compact, "abcdefghij");
        }
        let padded = c.with_pad(2);
        let padded_rows = padded.shared_lines(40);
        assert!(!Arc::ptr_eq(&first, &padded_rows));
        assert!(strip_ansi(&padded_rows.join("\n")).contains("  abcdefghij"));
    }

    #[test]
    fn test_renders_non_empty() {
        let c = UserMessageComponent::new("Hello world");
        let lines = c.render(40);
        assert!(!lines.is_empty());
        // Should contain the text (after stripping ANSI/OSC).
        let joined = strip_ansi(&lines.join("\n"));
        assert!(joined.contains("Hello"));
    }

    #[test]
    fn test_osc133_markers_present() {
        let c = UserMessageComponent::new("Hi");
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains(OSC133_ZONE_START), "missing zone-start A");
        assert!(joined.contains(OSC133_ZONE_END), "missing zone-end B");
        assert!(joined.contains(OSC133_ZONE_FINAL), "missing zone-final C");
    }

    #[test]
    fn test_cjk_no_panic() {
        let c = UserMessageComponent::new("你好世界");
        let lines = c.render(6);
        assert!(!lines.is_empty());
    }
}
