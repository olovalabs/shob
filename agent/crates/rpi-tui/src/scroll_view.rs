//! ScrollView - Scrollable container component.
//!
//! Implements scrolling with follow-end behavior for transcript display.
//! Follows the design from `tui-plan.md`.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::{Component, RenderedDocument};
use super::layout_node::{LayoutNode, LayoutNodeProvider, ScrollNode};

/// Follow mode for ScrollView.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowMode {
    /// No automatic follow behavior.
    None,
    /// Automatically scroll to end when new content is added while at end.
    #[default]
    End,
}

/// Overscroll behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverscrollMode {
    /// No overscroll.
    #[default]
    None,
    /// Chain unused scroll delta to parent scroll view.
    Chain,
    /// Contain scroll within this view.
    Contain,
}

/// Scrollbar visibility options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    /// Never show scrollbar.
    Hidden,
    /// Show scrollbar automatically when content exceeds viewport.
    #[default]
    Auto,
    /// Always show scrollbar.
    Always,
}

/// Options for ScrollView.
#[derive(Debug, Clone, Default)]
pub struct ScrollViewOptions {
    /// Axis for scrolling (only vertical is currently supported).
    pub axis: Option<ScrollAxis>,
    /// Follow mode for automatic scrolling.
    pub follow: FollowMode,
    /// Keep manual browsing detached from follow-end across layout changes.
    pub preserve_manual_position: bool,
    /// Designate this view as the fallback target for global scroll actions.
    pub primary: bool,
    /// Overscroll behavior.
    pub overscroll: OverscrollMode,
    /// Scrollbar visibility.
    pub scrollbar: ScrollbarMode,
    /// Delay in milliseconds before hiding auto scrollbar.
    pub scrollbar_hide_delay_ms: Option<u64>,
}

/// Scroll axis (only vertical is supported currently).
#[derive(Debug, Clone, Copy, Default)]
pub enum ScrollAxis {
    #[default]
    Vertical,
}

/// Internal state for ScrollView.
#[derive(Debug, Clone)]
struct ScrollViewState {
    /// Current scroll position (top row of visible area).
    scroll_top: usize,
    /// Whether we're following the end.
    is_following_end: bool,
    /// Viewport height (set during render).
    viewport_height: usize,
    /// Content height (set during render).
    content_height: usize,
    /// Whether follow was suppressed at the end.
    follow_suppressed_at_end: bool,
    /// Whether the scrollbar is temporarily visible (for auto mode).
    transient_scrollbar_visible: bool,
}

impl ScrollViewState {
    fn new(follow: FollowMode) -> Self {
        Self {
            scroll_top: 0,
            is_following_end: follow == FollowMode::End,
            viewport_height: 0,
            content_height: 0,
            follow_suppressed_at_end: false,
            transient_scrollbar_visible: false,
        }
    }
}

/// Immutable content and viewport position from one rendered frame.
#[derive(Clone)]
pub struct ScrollSnapshot {
    pub scroll_top: usize,
    pub content_width: usize,
    pub document: Arc<RenderedDocument>,
}

#[derive(Clone)]
struct ScrollRenderCache {
    content_width: usize,
    /// Cell geometry the document was rendered for.
    ///
    /// Image rows are derived from the terminal's cell size, so a document
    /// rendered for another geometry holds placements of the wrong height even
    /// when the content width is identical. Keying the cache on width alone
    /// would serve those stale rows to a pure scroll frame.
    cells: crate::terminal_image::CellDimensions,
    document: Arc<RenderedDocument>,
    /// Position used to slice this viewport, not a pending live scroll delta.
    scroll_top: usize,
}

/// ScrollView - A scrollable container.
///
/// Provides vertical scrolling with optional follow-end behavior.
/// In constrained layout, the child is measured at unbounded height
/// and clipped to the allocated viewport.
#[derive(Clone)]
pub struct ScrollView {
    child: Arc<dyn Component>,
    options: ScrollViewOptions,
    state: Arc<Mutex<ScrollViewState>>,
    /// Last fully rendered child. Pure scroll frames only change which rows
    /// are visible, so rebuilding the entire transcript would be wasted work.
    render_cache: Arc<Mutex<Option<ScrollRenderCache>>>,
    /// Callback to request a render.
    request_render_callback: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl ScrollView {
    /// Create a new ScrollView with the given child.
    pub fn new(child: Arc<dyn Component>, options: ScrollViewOptions) -> Self {
        let follow = options.follow;
        Self {
            child,
            options,
            state: Arc::new(Mutex::new(ScrollViewState::new(follow))),
            render_cache: Arc::new(Mutex::new(None)),
            request_render_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a simple ScrollView with default options.
    pub fn simple(child: Arc<dyn Component>) -> Self {
        Self::new(child, ScrollViewOptions::default())
    }

    /// Create a primary ScrollView with follow-end behavior.
    pub fn primary(child: Arc<dyn Component>) -> Self {
        Self::new(
            child,
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        )
    }

    /// Get the child component.
    pub fn child(&self) -> &Arc<dyn Component> {
        &self.child
    }

    /// Get the current scroll position.
    pub fn scroll_top(&self) -> usize {
        self.state.lock().map(|s| s.scroll_top).unwrap_or(0)
    }

    /// Check if following the end.
    pub fn is_following_end(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.is_following_end)
            .unwrap_or(false)
    }

    /// Whether this view can automatically follow newly appended content.
    pub fn follows_end(&self) -> bool {
        self.options.follow == FollowMode::End
    }

    /// Get the viewport height.
    pub fn viewport_height(&self) -> usize {
        self.state.lock().map(|s| s.viewport_height).unwrap_or(0)
    }

    /// Get the content height.
    pub fn content_height(&self) -> usize {
        self.state.lock().map(|s| s.content_height).unwrap_or(0)
    }

    /// Check if this is the primary scroll view.
    pub fn is_primary(&self) -> bool {
        self.options.primary
    }

    /// Whether two scroll views share the same scroll state.
    ///
    /// Layout passes hand out fresh `ScrollView` clones (each with its own
    /// `Arc`), so pointer identity cannot correlate a frame's boxes with the
    /// view the host created. The state `Arc` is the stable identity.
    pub fn same_state(&self, other: &ScrollView) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// The cached content rows the viewport was last rendered from.
    ///
    /// Empty when nothing was rendered yet or the cache was invalidated.
    pub fn content_lines(&self, width: usize) -> Option<Vec<String>> {
        let content_width = self.get_content_width(width);
        let cache = self.render_cache.lock().ok()?;
        let cache = cache.as_ref()?;
        if cache.content_width != content_width
            || cache.cells != crate::terminal_image::effective_cell_dimensions()
        {
            return None;
        }
        let height = cache.document.len();
        Some(cache.document.lines(0, height))
    }

    /// Capture the cached document without copying historical rows.
    pub fn snapshot(&self, width: usize) -> Option<ScrollSnapshot> {
        let content_width = self.get_content_width(width);
        let cache = self.render_cache.lock().ok()?;
        let cache = cache.as_ref()?;
        if cache.content_width != content_width {
            return None;
        }
        Some(ScrollSnapshot {
            scroll_top: cache.scroll_top,
            content_width,
            document: cache.document.clone(),
        })
    }

    /// Check if scrollbar is visible.
    pub fn is_scrollbar_visible(&self) -> bool {
        match self.options.scrollbar {
            ScrollbarMode::Always => self.viewport_height() > 0,
            ScrollbarMode::Auto => {
                self.content_height() > self.viewport_height()
                    && self
                        .state
                        .lock()
                        .map(|s| s.transient_scrollbar_visible)
                        .unwrap_or(false)
            }
            ScrollbarMode::Hidden => false,
        }
    }

    /// Get the content width (accounting for scrollbar).
    pub fn get_content_width(&self, width: usize) -> usize {
        if self.options.scrollbar == ScrollbarMode::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    /// Scroll by the given number of lines.
    ///
    /// Returns the unused delta if scrolling hit the boundary.
    /// This enables scroll chaining for nested scroll views.
    pub fn scroll_by(&self, delta: i32) -> i32 {
        if delta == 0 {
            return 0;
        }

        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);

            // If following end, start from the end position
            let start = if state.is_following_end {
                max_scroll
            } else {
                state.scroll_top
            };

            let new_top = if delta > 0 {
                start.saturating_add(delta as usize)
            } else {
                start.saturating_sub((-delta) as usize)
            };

            let clamped = new_top.min(max_scroll);
            let moved = (clamped as i32) - (start as i32);

            state.scroll_top = clamped;
            state.is_following_end =
                self.options.follow == FollowMode::End && clamped >= max_scroll;
            state.follow_suppressed_at_end = false;

            // Mark scrollbar activity
            if moved != 0 {
                state.transient_scrollbar_visible = true;
            }

            delta - moved
        } else {
            delta
        }
    }

    /// Pause follow-end at a frame's position without requesting a render.
    /// Returns whether follow-end was active before pausing.
    pub fn pause_follow_at(&self, position: usize) -> bool {
        if let Ok(mut state) = self.state.lock() {
            let was_following_end = state.is_following_end;
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            state.scroll_top = position.min(max_scroll);
            state.is_following_end = false;
            state.follow_suppressed_at_end = true;
            was_following_end
        } else {
            false
        }
    }

    /// Restore a temporary click pause unless explicit navigation superseded
    /// it. Layout clamping does not count as user navigation. No render callback:
    /// callers may be cancelling a press while already assembling a frame.
    pub(crate) fn resume_follow_after_click(&self) {
        if let Ok(mut state) = self.state.lock() {
            if self.options.follow == FollowMode::End && state.follow_suppressed_at_end {
                state.scroll_top = state.content_height.saturating_sub(state.viewport_height);
                state.is_following_end = true;
                state.follow_suppressed_at_end = false;
            }
        }
    }

    /// Scroll to a specific position.
    pub fn scroll_to(&self, position: usize) {
        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            state.scroll_top = position.min(max_scroll);

            if self.options.follow == FollowMode::End {
                state.is_following_end = state.scroll_top >= max_scroll;
            }
            state.follow_suppressed_at_end = false;
            state.transient_scrollbar_visible = true;
        }
        self.request_render();
    }

    /// Scroll to the start.
    pub fn scroll_to_start(&self) {
        if let Ok(mut state) = self.state.lock() {
            let content_fits = state.content_height <= state.viewport_height;
            let following_end = self.options.follow == FollowMode::End && content_fits;
            let changed = state.scroll_top != 0 || state.is_following_end != following_end;
            state.scroll_top = 0;
            state.is_following_end = following_end;
            state.follow_suppressed_at_end = false;

            if changed {
                state.transient_scrollbar_visible = true;
            }
        }
        self.request_render();
    }

    /// Scroll to the end.
    pub fn scroll_to_end(&self) {
        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            let changed = state.scroll_top != max_scroll || !state.is_following_end;
            state.scroll_top = max_scroll;
            state.is_following_end = self.options.follow == FollowMode::End;
            state.follow_suppressed_at_end = false;

            if changed {
                state.transient_scrollbar_visible = true;
            }
        }
        self.request_render();
    }

    /// Update layout information (called by the layout system).
    pub fn update_layout(
        &self,
        content_height: usize,
        viewport_height: usize,
        request_render: Option<Arc<dyn Fn() + Send + Sync>>,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.content_height = content_height;
            state.viewport_height = viewport_height;

            // Store the render callback
            if let Some(cb) = request_render {
                *self.request_render_callback.lock().unwrap() = Some(cb);
            }

            // If following end, adjust scroll position
            if self.options.follow == FollowMode::End && state.is_following_end {
                let max_scroll = content_height.saturating_sub(viewport_height);
                state.scroll_top = max_scroll;
            }

            // Ensure scroll position is valid
            let max_scroll = content_height.saturating_sub(viewport_height);
            state.scroll_top = state.scroll_top.min(max_scroll);

            // Update follow state. Explicit pauses survive layout changes,
            // including a viewport that temporarily fits all content.
            if self.options.follow == FollowMode::End
                && !self.options.preserve_manual_position
                && state.scroll_top == max_scroll
                && !state.follow_suppressed_at_end
            {
                state.is_following_end = true;
            }

            // Hide scrollbar if content fits
            if content_height <= viewport_height {
                state.transient_scrollbar_visible = false;
            }
        }
    }

    /// Request a render from the layout system.
    fn request_render(&self) {
        if let Ok(cb) = self.request_render_callback.lock() {
            if let Some(callback) = cb.as_ref() {
                callback();
            }
        }
    }

    /// Render fresh child content, applying scroll for a fixed viewport.
    ///
    /// Normal UI updates use this path so streamed text and mutable tool
    /// components are reflected immediately. The result also primes the
    /// cache used by subsequent pure scroll frames.
    pub fn render_with_viewport(&self, width: usize, height: usize) -> Vec<String> {
        self.render_with_viewport_impl(width, height, false)
    }

    /// Apply the viewport to the last rendered child content when possible.
    /// A width change automatically falls back to a fresh render.
    pub fn render_with_cached_content(&self, width: usize, height: usize) -> Vec<String> {
        self.render_with_viewport_impl(width, height, true)
    }

    /// Document rendered for the last viewport pass.
    ///
    /// The layout uses it to find placements that scrolled above the viewport:
    /// a kitty escape lives on the image's first row, which the visible slice
    /// no longer contains, so the placement has to be re-anchored to the first
    /// visible row and cropped to the rows that are still on screen.
    pub(crate) fn rendered_document(&self) -> Option<Arc<RenderedDocument>> {
        self.render_cache
            .lock()
            .ok()
            .and_then(|cache| cache.as_ref().map(|cache| cache.document.clone()))
    }

    fn render_with_viewport_impl(
        &self,
        width: usize,
        height: usize,
        reuse_cached_content: bool,
    ) -> Vec<String> {
        let content_width = self.get_content_width(width);
        let cells = crate::terminal_image::effective_cell_dimensions();
        let cached = if reuse_cached_content {
            self.render_cache
                .lock()
                .ok()
                .and_then(|cache| cache.clone())
                .filter(|cache| cache.content_width == content_width && cache.cells == cells)
        } else {
            None
        };
        let document = cached
            .map(|cache| cache.document)
            .unwrap_or_else(|| Arc::new(self.child.render_document(content_width)));
        let content_height = document.len();

        // Update state
        self.update_layout(content_height, height, None);

        let scroll_top = self.scroll_top();

        // Extract visible portion
        let visible = document.lines(scroll_top, height);

        // Pad to fill viewport if needed
        let mut visible = visible;
        while visible.len() < height {
            visible.push(String::new());
        }

        // Scrollbar column: a positioned thumb on the right edge so scrolling
        // has visible feedback (the old code was a TODO no-op — scrolling
        // moved the window with no indicator). `Always` reserves the column in
        // get_content_width; `Auto` overlays it while a scroll is active.
        if self.is_scrollbar_visible() {
            let total = content_height.max(1);
            let viewport = height.max(1);
            let thumb = if total <= viewport {
                viewport
            } else {
                (viewport * viewport / total).max(1)
            };
            let thumb_top = if total > viewport {
                scroll_top * (viewport.saturating_sub(thumb)) / (total - viewport)
            } else {
                0
            };
            for (i, line) in visible.iter_mut().enumerate() {
                let on = i >= thumb_top && i < thumb_top + thumb;
                if width > 0 && !line.contains("\x1b_G") && !line.contains("\x1b]1337;File=") {
                    let cell = if on {
                        self.scrollbar_style(" ")
                    } else {
                        " ".to_owned()
                    };
                    *line = crate::layout::composite_tui_line(line, &cell, width - 1, 1, width);
                }
            }
        }

        // Cache exactly the source and offset used for the returned rows.
        // A wheel event may have changed live state while rendering them.
        if let Ok(mut cache) = self.render_cache.lock() {
            *cache = Some(ScrollRenderCache {
                content_width,
                cells,
                document,
                scroll_top,
            });
        }
        visible
    }

    /// Get the scrollbar style (can be customized).
    pub fn scrollbar_style(&self, text: &str) -> String {
        format!("\x1b[100m{}\x1b[49m", text)
    }
}

impl Component for ScrollView {
    fn render(&self, width: usize) -> Vec<String> {
        // Unconstrained render: show all content
        let content_width = self.get_content_width(width);
        self.child.render(content_width)
    }

    fn invalidate(&self) {
        if let Ok(mut cache) = self.render_cache.lock() {
            *cache = None;
        }
        self.child.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl LayoutNodeProvider for ScrollView {
    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Scroll(ScrollNode::new(self.child.clone())))
    }
}

/// Render a ScrollView with a fixed viewport height.
pub fn render_scroll_view(scroll_view: &ScrollView, width: usize, height: usize) -> Vec<String> {
    scroll_view.render_with_viewport(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;

    struct SharedHistory(Arc<Vec<String>>);

    impl Component for SharedHistory {
        fn render(&self, _: usize) -> Vec<String> {
            panic!("fullscreen must not flatten historical rows");
        }
        fn render_document(&self, _: usize) -> RenderedDocument {
            RenderedDocument::from_shared(self.0.clone())
        }
        fn invalidate(&self) {}
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MutableRow(Mutex<String>);

    impl Component for MutableRow {
        fn render(&self, _: usize) -> Vec<String> {
            vec![self.0.lock().unwrap().clone()]
        }
        fn invalidate(&self) {}
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn snapshot_shares_document_and_survives_scroll_and_cache_replacement() {
        let rows = Arc::new((0..50).map(|i| format!("row {i}")).collect());
        let scroll = ScrollView::new(
            Arc::new(SharedHistory(rows)),
            ScrollViewOptions {
                scrollbar: ScrollbarMode::Always,
                ..Default::default()
            },
        );
        assert!(scroll.snapshot(40).is_none());
        scroll.render_with_viewport(40, 10);
        let snapshot = scroll.snapshot(40).unwrap();
        assert_eq!(snapshot.content_width, 39);
        assert_eq!(snapshot.scroll_top, 40);
        assert!(Arc::ptr_eq(
            &snapshot.document,
            &scroll
                .render_cache
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .document
        ));
        assert!(scroll.snapshot(39).is_none());

        scroll.scroll_by(-10);
        assert_eq!(scroll.scroll_top(), 30);
        assert_eq!(
            scroll.snapshot(40).unwrap().scroll_top,
            40,
            "pending scroll has not painted yet"
        );
        scroll.render_with_cached_content(40, 10);
        assert_eq!(scroll.snapshot(40).unwrap().scroll_top, 30);
        assert_eq!(snapshot.scroll_top, 40);
        scroll.invalidate();
        assert!(scroll.snapshot(40).is_none());
        scroll.render_with_viewport(20, 10);
        let replacement = scroll.snapshot(20).unwrap();
        assert!(!Arc::ptr_eq(&snapshot.document, &replacement.document));
        assert_eq!(snapshot.content_width, 39);
        assert_eq!(snapshot.document.lines(snapshot.scroll_top, 1), ["row 40"]);
    }

    #[test]
    fn snapshot_keeps_old_content_after_mutable_child_refresh() {
        let child = Arc::new(MutableRow(Mutex::new("original".into())));
        let scroll = ScrollView::simple(child.clone());
        scroll.render_with_viewport(40, 1);
        let snapshot = scroll.snapshot(40).unwrap();
        *child.0.lock().unwrap() = "replacement".into();
        scroll.render_with_viewport(40, 1);
        assert_eq!(snapshot.document.lines(0, 1), ["original"]);
        assert_eq!(
            scroll.snapshot(40).unwrap().document.lines(0, 1),
            ["replacement"]
        );
    }

    #[test]
    fn fresh_frames_share_history_and_refresh_arbitrary_mutable_children() {
        for size in [1, 1_000, 10_000] {
            let rows = Arc::new(vec!["historical row".repeat(16); size]);
            let transcript = Arc::new(crate::Container::new());
            transcript.add_child(Arc::new(SharedHistory(rows.clone())));
            let live = Arc::new(MutableRow(Mutex::new("first update".into())));
            let live_component: Arc<dyn Component> = live.clone();
            transcript.add_child(live_component.clone());
            let wrapper = Arc::new(crate::Container::new());
            wrapper.add_child(transcript.clone());
            let scroll = ScrollView::new(
                wrapper,
                ScrollViewOptions {
                    scrollbar: ScrollbarMode::Hidden,
                    preserve_manual_position: true,
                    ..Default::default()
                },
            );
            assert_eq!(scroll.render_with_viewport(40, 1), ["first update"]);
            // Original handle, SharedHistory, and cached document all retain
            // the same historical row allocation; no strings were flattened.
            assert_eq!(Arc::strong_count(&rows), 3);
            *live.0.lock().unwrap() = "second update".into();
            assert_eq!(scroll.render_with_viewport(40, 1), ["second update"]);
            assert_eq!(Arc::strong_count(&rows), 3);
            scroll.scroll_to_start();
            let position = scroll.scroll_top();
            *live.0.lock().unwrap() = "third update".into();
            scroll.render_with_viewport(40, 1);
            assert_eq!(scroll.scroll_top(), position);
            assert!(!scroll.is_following_end());
            scroll.scroll_to_end();
            assert_eq!(scroll.render_with_cached_content(40, 1), ["third update"]);
            transcript.remove_child(&live_component);
            scroll.render_with_viewport(40, 1);
            transcript.add_child(live_component);
            assert_eq!(scroll.render_with_viewport(20, 1), ["third update"]);
            scroll.invalidate();
            *live.0.lock().unwrap() = "after invalidation".into();
            assert_eq!(
                scroll.render_with_cached_content(20, 1),
                ["after invalidation"]
            );
            assert_eq!(Arc::strong_count(&rows), 3);
        }
    }

    /// A pure scroll frame reuses the last rendered document, but only for the
    /// geometry it was rendered at: image rows are derived from the cell size, so
    /// serving a document rendered for another geometry would keep the previous
    /// row heights in the transcript.
    ///
    /// The counter proves the child was actually re-rendered — asserting only on
    /// the rows would pass on a document that happened to look the same.
    #[test]
    fn cached_content_is_dropped_when_only_the_cell_geometry_changed() {
        struct CountingChild {
            renders: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Component for CountingChild {
            fn render(&self, _: usize) -> Vec<String> {
                self.render_document(0).lines(0, usize::MAX)
            }
            fn render_document(&self, _: usize) -> RenderedDocument {
                self.renders
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                RenderedDocument::from_lines(vec!["row".into()])
            }
            fn invalidate(&self) {}
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        // The capability cache is process-wide, so this probe has to hold the
        // shared lock: without it a parallel test can observe the geometry swap
        // mid-assertion (or this one can observe theirs).
        let _g = crate::terminal_image::capability_test_lock();
        let saved = crate::terminal_image::get_capabilities();
        struct Restore(crate::terminal_image::TerminalCapabilities);
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::terminal_image::set_capabilities(self.0.clone());
            }
        }
        let _restore = Restore(saved);

        crate::terminal_image::set_cell_dimensions(10, 20);
        let renders = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scroll = ScrollView::new(
            Arc::new(CountingChild {
                renders: renders.clone(),
            }),
            ScrollViewOptions {
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        );

        scroll.render_with_cached_content(40, 1);
        assert_eq!(renders.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Same width and geometry: the cache is what makes a pure scroll free.
        scroll.render_with_cached_content(40, 1);
        assert_eq!(renders.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Same width, new cell geometry: the cached document is stale.
        crate::terminal_image::set_cell_dimensions(10, 10);
        scroll.render_with_cached_content(40, 1);
        assert_eq!(renders.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    #[ignore = "manual render-cost comparison; run with --ignored --nocapture"]
    fn compare_flat_and_shared_long_transcript_frames() {
        use std::hint::black_box;
        use std::time::Instant;

        for count in [10, 200] {
            let transcript = crate::Container::new();
            for index in 0..count {
                transcript.add_child(Arc::new(crate::UserMessageComponent::new(format!(
                    "prompt {index}\n{}",
                    "historical prompt text\n".repeat(10)
                ))));
                let response = Arc::new(crate::AssistantMessageComponent::default());
                response.update_text(&"historical **response** with `code`\n".repeat(20));
                transcript.add_child(response);
            }
            let live = Arc::new(crate::AssistantMessageComponent::default());
            transcript.add_child(live.clone());
            live.update_text("initial live output");
            let expected = transcript.render(80);
            let document = transcript.render_document(80);
            assert_eq!(document.lines(0, document.len()), expected);
            let rows = expected.len();
            let frames = 100;
            let started = Instant::now();
            for index in 0..frames {
                live.update_text(&format!("live output {index}"));
                let lines = transcript.render(80);
                black_box(&lines[lines.len().saturating_sub(20)..]);
            }
            let flat = started.elapsed();
            let started = Instant::now();
            for index in 0..frames {
                live.update_text(&format!("live output {index}"));
                let document = transcript.render_document(80);
                black_box(document.lines(document.len().saturating_sub(20), 20));
            }
            let shared = started.elapsed();
            eprintln!("{count} pairs / {rows} rows / {frames} fresh frames: flat={flat:?}, shared={shared:?}");
        }
    }

    #[test]
    fn shared_message_updates_match_legacy_rendering() {
        let transcript = Arc::new(crate::Container::new());
        transcript.add_child(Arc::new(crate::UserMessageComponent::new("user prompt")));
        let assistant = Arc::new(crate::AssistantMessageComponent::default());
        assistant.update_text("first **response**");
        assistant.set_trailing_spacing(1);
        transcript.add_child(assistant.clone());
        let bash = Arc::new(crate::BashExecutionComponent::new("echo result"));
        transcript.add_child(bash.clone());
        let scroll = ScrollView::new(
            transcript.clone(),
            ScrollViewOptions {
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        );
        for width in [80, 30] {
            assistant.update_text("latest **response**");
            bash.append_output("latest tool output");
            bash.set_complete(Some(0), false, crate::BashTruncation::default());
            let expected = transcript.render(width);
            let actual = scroll.render_with_viewport(width, expected.len());
            assert_eq!(actual, expected);
            assert!(crate::strip_ansi(&actual.join("\n")).contains("latest response"));
            assert!(crate::strip_ansi(&actual.join("\n")).contains("latest tool output"));
        }
    }

    #[test]
    fn test_scroll_view_basic() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3\nLine 4\nLine 5", 0, 0));
        let scroll = ScrollView::simple(text);

        assert_eq!(scroll.scroll_top(), 0);
        assert!(scroll.is_following_end());
    }

    #[test]
    fn test_scroll_by() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3\nLine 4\nLine 5", 0, 0));
        let scroll = ScrollView::new(
            text,
            ScrollViewOptions {
                follow: FollowMode::None,
                ..Default::default()
            },
        );

        // Simulate content and viewport
        scroll.update_layout(5, 2, None);

        let unused = scroll.scroll_by(1);
        assert_eq!(unused, 0);
        assert_eq!(scroll.scroll_top(), 1);
    }

    #[test]
    fn test_scroll_to_end() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3", 0, 0));
        let scroll = ScrollView::new(
            text,
            ScrollViewOptions {
                follow: FollowMode::End,
                ..Default::default()
            },
        );

        scroll.update_layout(3, 1, None);
        scroll.scroll_to_end();

        assert!(scroll.is_following_end());
        assert_eq!(scroll.scroll_top(), 2);
    }

    #[test]
    fn test_scroll_to_start() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3", 0, 0));
        let scroll = ScrollView::simple(text);

        scroll.update_layout(3, 1, None);
        scroll.scroll_by(2);
        scroll.scroll_to_start();

        assert_eq!(scroll.scroll_top(), 0);
        assert!(!scroll.is_following_end());
    }
}
#[cfg(test)]
mod scroll_tests {
    use super::*;
    use crate::Container;
    use crate::Text;

    fn tall_scroll() -> (Arc<ScrollView>, Arc<Container>) {
        let inner = Arc::new(Container::new());
        for i in 0..50 {
            inner.add_child(Arc::new(Text::new(format!("line {i:02}"), 0, 0)));
        }
        let sv = Arc::new(ScrollView::new(
            inner.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        (sv, inner)
    }

    #[test]
    fn scroll_by_changes_visible_window() {
        let (sv, _inner) = tall_scroll();
        let before = sv.render_with_viewport(40, 10);
        assert!(
            before[0].contains("line 40"),
            "following end shows tail: {:?}",
            before[0]
        );
        // Scroll up 10 lines.
        let unused = sv.scroll_by(-10);
        assert_eq!(unused, 0);
        let after = sv.render_with_viewport(40, 10);
        assert!(
            after[0].contains("line 30"),
            "scroll up should reveal older lines, got {:?}",
            after[0]
        );
        // Scroll back down to the end.
        sv.scroll_by(10);
        let back = sv.render_with_viewport(40, 10);
        assert!(back[0].contains("line 40"));
    }

    #[test]
    fn manual_scroll_up_stays_put_until_user_returns_to_end() {
        let (sv, inner) = tall_scroll();
        sv.render_with_viewport(40, 10); // sync layout
        assert!(sv.is_following_end());
        sv.scroll_by(-3);
        assert!(!sv.is_following_end(), "manual scroll leaves follow mode");

        let position = sv.scroll_top();
        inner.add_child(Arc::new(Text::new("new output", 0, 0)));
        sv.render_with_viewport(40, 10);
        assert_eq!(
            sv.scroll_top(),
            position,
            "new output must not steal the viewport"
        );
        assert!(!sv.is_following_end());

        sv.scroll_to_end();
        assert!(sv.is_following_end());
        let previous_end = sv.scroll_top();
        inner.add_child(Arc::new(Text::new("more output", 0, 0)));
        sv.render_with_viewport(40, 10);
        assert!(
            sv.scroll_top() > previous_end,
            "tail should advance after follow is restored"
        );
    }

    #[test]
    fn detached_scroll_does_not_resume_follow_when_reflow_temporarily_fits() {
        let text = Arc::new(Text::new("line\n".repeat(30), 0, 0));
        let sv = ScrollView::new(
            text.clone(),
            ScrollViewOptions {
                preserve_manual_position: true,
                ..Default::default()
            },
        );
        sv.render_with_viewport(40, 10);
        sv.scroll_by(-5);
        assert!(!sv.is_following_end());
        sv.render_with_viewport(40, 100);
        assert!(!sv.is_following_end());
        text.set_text("line\n".repeat(200));
        sv.render_with_viewport(40, 10);
        assert_eq!(sv.scroll_top(), 0);
        assert!(!sv.is_following_end());
        sv.scroll_to_end();
        assert!(sv.is_following_end());
    }

    #[test]
    fn pause_follow_survives_append_resize_and_temporary_fit() {
        let (sv, inner) = tall_scroll();
        sv.render_with_viewport(40, 10);
        let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count = callbacks.clone();
        sv.update_layout(
            50,
            10,
            Some(Arc::new(move || {
                callback_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
        );
        assert!(sv.pause_follow_at(usize::MAX));
        assert_eq!(sv.scroll_top(), 40, "pause clamps to the current maximum");
        assert!(!sv.pause_follow_at(30));
        assert_eq!(callbacks.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!sv.is_following_end());

        inner.add_child(Arc::new(Text::new("new output", 0, 0)));
        sv.render_with_viewport(40, 10);
        assert_eq!(sv.scroll_top(), 30);
        assert!(!sv.is_following_end());
        // A dock resize first clamps the paused position to the end, then
        // temporarily fits everything. Neither may re-enable follow-end.
        sv.render_with_viewport(40, 25);
        assert_eq!(sv.scroll_top(), 26);
        assert!(!sv.is_following_end());
        sv.render_with_viewport(40, 100);
        assert_eq!(sv.scroll_top(), 0);
        assert!(!sv.is_following_end());
        inner.add_child(Arc::new(Text::new("more output", 0, 0)));
        sv.render_with_viewport(20, 10);
        assert_eq!(sv.scroll_top(), 0);
        assert!(!sv.is_following_end());

        sv.scroll_to_end();
        assert!(sv.is_following_end());
        let previous_end = sv.scroll_top();
        inner.add_child(Arc::new(Text::new("followed output", 0, 0)));
        sv.render_with_viewport(20, 10);
        assert!(sv.scroll_top() > previous_end);
    }

    #[test]
    fn scroll_to_start_keeps_follow_when_content_fits() {
        let text = Arc::new(Text::new("short", 0, 0));
        let sv = ScrollView::simple(text);
        sv.update_layout(1, 10, None);

        sv.scroll_to_start();

        assert!(sv.is_following_end());
    }
}
