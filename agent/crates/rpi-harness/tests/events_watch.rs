//! M5e integration — harness event bus. Mirrors the two cases in
//! `packages/agent/test/harness/events.test.ts`:
//! 1. "delivers matching events to direct listeners and watchers" — a typed
//!    `on("run_start")` listener receives only matching events (and stops after
//!    `off`); a watch (started before emits) receives every event (run_start +
//!    run_end + run_start).
//! 2. "captures a snapshot without an event gap, then flushes and delivers live
//!    events" — the watch's snapshot closure emits `run_start`, which must be
//!    buffered (not lost — no event gap); `start` flushes it; subsequent emits
//!    go live; `unsubscribe` stops delivery.
//!
//! These exercise the PUBLIC bus surface (no crate-private `EmitterHandle`).
//! Case 2 emits from inside the snapshot closure by capturing a shared `&bus`
//! borrow alongside `watch`'s own `&self` borrow — two shared borrows, which the
//! borrow checker permits.

use std::sync::{Arc, Mutex};

use rpi_harness::events::{
    HarnessEvent, HarnessEventBus, RunEndEvent, RunEndOutcome, RunStartEvent, WatchListener,
};

fn run_start() -> HarnessEvent {
    HarnessEvent::RunStart(RunStartEvent {
        lane: "main".into(),
        run_id: "run-1".into(),
    })
}

fn run_end() -> HarnessEvent {
    HarnessEvent::RunEnd(RunEndEvent {
        lane: "main".into(),
        run_id: "run-1".into(),
        outcome: RunEndOutcome::Completed,
        leaf_id: "entry-1".into(),
    })
}

#[test]
fn delivers_matching_events_to_direct_listeners_and_watchers() {
    let events = HarnessEventBus::new();
    let direct: Arc<Mutex<Vec<RunStartEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let watch_events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let direct_clone = direct.clone();
    let off = events.on::<RunStartEvent, _>(move |e| direct_clone.lock().unwrap().push(e.clone()));
    let watch_events_clone = watch_events.clone();
    let mut watch = events.watch(|| ());
    let listener: WatchListener = Arc::new(move |event: &HarnessEvent| {
        watch_events_clone.lock().unwrap().push(event.clone())
    });
    watch.start(listener);

    events.emit(&run_start());
    events.emit(&run_end());
    drop(off);
    events.emit(&run_start());

    assert_eq!(direct.lock().unwrap().len(), 1);
    assert_eq!(
        watch_events.lock().unwrap().as_slice(),
        &[run_start(), run_end(), run_start()]
    );
}

#[test]
fn snapshot_then_flush_then_live() {
    // Mirrors TS "captures a snapshot without an event gap, then flushes and
    // delivers live events". The snapshot closure emits run_start, which must
    // be buffered (the watch is not yet started). `start` flushes the buffer in
    // order; subsequent emits go live; `unsubscribe` halts delivery.
    let events = HarnessEventBus::new();
    let expected_snapshot = Arc::new(42i32);
    let expected_snapshot_for_closure = expected_snapshot.clone();
    // `events` is borrowed by `watch(&self)` and by the closure (two shared
    // borrows) — both immutable, so the borrow checker accepts it.
    let mut watch = events.watch(|| {
        events.emit(&run_start());
        *expected_snapshot_for_closure
    });
    let received: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    // Snapshot captured (TS asserts `watch.snapshot === expected`).
    assert_eq!(*watch.snapshot(), *expected_snapshot);
    assert!(received.lock().unwrap().is_empty());
    let live: WatchListener =
        Arc::new(move |event: &HarnessEvent| received_clone.lock().unwrap().push(event.clone()));
    watch.start(live);
    // The run_start emitted inside the snapshot closure is flushed by `start`.
    assert_eq!(received.lock().unwrap().as_slice(), &[run_start()]);
    events.emit(&run_end());
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[run_start(), run_end()]
    );
    watch.unsubscribe();
    events.emit(&run_start());
    // After unsubscribe no further events arrive.
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[run_start(), run_end()]
    );
}
