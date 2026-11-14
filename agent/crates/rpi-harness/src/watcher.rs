//! Live event watching for the harness surface.
//!
//! Mirrors the TS `AgentHarness.watch` / `watchSession` helpers. Rust's
//! [`WatchHandle`](crate::events::WatchHandle) already provides the delivery
//! machinery (buffer-then-start, ordered flush, no auto-unsubscribe on drop).
//! This module wraps that handle in an owning RAII guard so the idiomatic Rust
//! shape is "drop to stop watching" while the underlying bus semantics stay
//! faithful.
//!
//! Unlike the TS handle (which intentionally leaks the watch until an explicit
//! `unsubscribe`), [`HarnessWatcher`] unsubscribes on drop — a leaked listener
//! holding an `Arc` would keep the bus entry alive for the process lifetime,
//! which is never what a Rust caller wants. Callers that need the TS behavior
//! can [`HarnessWatcher::leak`] the guard.

use crate::events::{WatchHandle, WatchListener};

/// An owning handle to an active harness event watch.
///
/// Dropping it unsubscribes the listener and clears its buffer. Use
/// [`HarnessWatcher::into_inner`] to recover the raw [`WatchHandle`] (e.g. to
/// read the registration snapshot) or [`HarnessWatcher::leak`] to keep the
/// watch alive past the guard.
pub struct HarnessWatcher {
    handle: Option<WatchHandle<()>>,
}

impl HarnessWatcher {
    /// Wrap a started [`WatchHandle`].
    pub(crate) fn new(handle: WatchHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// Whether the watch is still subscribed.
    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    /// Stop watching immediately (idempotent).
    pub fn unsubscribe(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.unsubscribe();
        }
    }

    /// Keep the watch alive for the rest of the process, returning the raw
    /// handle. Mirrors the TS behavior of dropping a handle without
    /// unsubscribing.
    pub fn leak(mut self) -> WatchHandle<()> {
        self.handle
            .take()
            .expect("harness watcher already released")
    }

    /// Recover the raw handle without unsubscribing.
    pub fn into_inner(mut self) -> WatchHandle<()> {
        self.handle
            .take()
            .expect("harness watcher already released")
    }
}

impl Drop for HarnessWatcher {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Build a [`WatchListener`] from a closure — the `Arc<dyn Fn>` shape
/// [`WatchHandle::start`] consumes.
pub fn watch_listener<F>(f: F) -> WatchListener
where
    F: Fn(&crate::events::HarnessEvent) + Send + Sync + 'static,
{
    std::sync::Arc::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{HarnessEvent, HarnessEventBus};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn watcher_receives_events_and_stops_on_drop() {
        let bus = HarnessEventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_listener = count.clone();

        let mut watch = bus.watch(|| ());
        watch.start(watch_listener(move |_event: &HarnessEvent| {
            count_for_listener.fetch_add(1, Ordering::SeqCst);
        }));
        let watcher = HarnessWatcher::new(watch);

        bus.emit(&HarnessEvent::RunStart(crate::events::RunStartEvent {
            lane: "main".into(),
            run_id: "r1".into(),
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);

        drop(watcher);

        // After the watcher is dropped the listener is unsubscribed.
        bus.emit(&HarnessEvent::RunStart(crate::events::RunStartEvent {
            lane: "main".into(),
            run_id: "r2".into(),
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn leak_keeps_watch_alive() {
        let bus = HarnessEventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_listener = count.clone();

        let mut watch = bus.watch(|| ());
        watch.start(watch_listener(move |_event: &HarnessEvent| {
            count_for_listener.fetch_add(1, Ordering::SeqCst);
        }));
        let leaked = HarnessWatcher::new(watch).leak();

        bus.emit(&HarnessEvent::RunStart(crate::events::RunStartEvent {
            lane: "main".into(),
            run_id: "r1".into(),
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);

        drop(leaked);
        // The bus keeps the watch alive (TS semantics).
        bus.emit(&HarnessEvent::RunStart(crate::events::RunStartEvent {
            lane: "main".into(),
            run_id: "r2".into(),
        }));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
