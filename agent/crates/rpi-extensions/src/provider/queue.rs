//! Bounded synchronous-producer/async-consumer bridge. Payload is accounted in
//! incoming JSON bytes, not allocator overhead or total process memory.
use super::{ProviderControlState, StreamChunk};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

#[cfg(test)]
static PEAK_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(test, feature = "provider-performance"))]
pub(super) fn peak_occupancy() -> (usize, usize) {
    use std::sync::atomic::Ordering;
    (
        PEAK_COUNT.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed),
    )
}

struct State {
    chunks: VecDeque<(StreamChunk, usize)>,
    bytes: usize,
    producer_open: bool,
    consumer_open: bool,
    cancelled: bool,
    peak_count: usize,
    peak_bytes: usize,
}

// Only combine deltas that are already queued. This introduces no flush timer:
// a lone chunk is delivered immediately. Authoritative snapshots, metadata and
// unfamiliar fields stay separate, as do changes of content block or kind.
const MAX_DELTA_BATCH_BYTES: usize = 4096;

fn delta_identity(chunk: &StreamChunk) -> Option<(&str, u64, &str)> {
    let object = chunk.as_object()?;
    if object.len() != 3 {
        return None;
    }
    let kind = object.get("type")?.as_str()?;
    if !matches!(kind, "text_delta" | "thinking_delta") {
        return None;
    }
    Some((
        kind,
        object.get("contentIndex")?.as_u64()?,
        object.get("delta")?.as_str()?,
    ))
}

impl State {
    fn pop_batch(&mut self) -> Option<StreamChunk> {
        let (mut chunk, bytes) = self.chunks.pop_front()?;
        self.bytes -= bytes;
        while let (Some((kind, index, delta)), Some((next, _))) =
            (delta_identity(&chunk), self.chunks.front())
        {
            let Some((next_kind, next_index, next_delta)) = delta_identity(next) else {
                break;
            };
            if kind != next_kind
                || index != next_index
                || delta.len().saturating_add(next_delta.len()) > MAX_DELTA_BATCH_BYTES
            {
                break;
            }
            let (mut next, bytes) = self.chunks.pop_front().unwrap();
            self.bytes -= bytes;
            let serde_json::Value::String(next_delta) =
                next.as_object_mut().unwrap().remove("delta").unwrap()
            else {
                unreachable!("delta_identity checked the string")
            };
            if let serde_json::Value::String(delta) = &mut chunk["delta"] {
                delta.push_str(&next_delta);
            }
        }
        Some(chunk)
    }
}
pub(super) struct Queue {
    state: Mutex<State>,
    capacity: usize,
    byte_limit: usize,
    writable: Condvar,
    readable: Notify,
}
pub(super) struct Sender(Arc<Queue>);
pub(super) struct Receiver {
    queue: Arc<Queue>,
    control: Arc<ProviderControlState>,
}

pub(super) fn channel(
    capacity: usize,
    byte_limit: usize,
    control: Arc<ProviderControlState>,
) -> (Sender, Receiver) {
    assert!(capacity > 0 && byte_limit > 0);
    let queue = Arc::new(Queue {
        state: Mutex::new(State {
            chunks: VecDeque::new(),
            bytes: 0,
            producer_open: true,
            consumer_open: true,
            cancelled: false,
            peak_count: 0,
            peak_bytes: 0,
        }),
        capacity,
        byte_limit,
        writable: Condvar::new(),
        readable: Notify::new(),
    });
    *control.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&queue));
    (Sender(Arc::clone(&queue)), Receiver { queue, control })
}

impl Queue {
    pub(super) fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        self.writable.notify_all();
        self.readable.notify_one();
    }
}
impl Sender {
    pub(super) fn send(&self, chunk: StreamChunk, bytes: usize) -> Result<(), ()> {
        let weight = bytes.max(1).min(self.0.byte_limit);
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.consumer_open
            && !state.cancelled
            && (state.chunks.len() == self.0.capacity || weight > self.0.byte_limit - state.bytes)
        {
            state = self
                .0
                .writable
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if !state.consumer_open || state.cancelled {
            return Err(());
        }
        state.bytes += weight;
        state.chunks.push_back((chunk, weight));
        state.peak_count = state.peak_count.max(state.chunks.len());
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            PEAK_COUNT.fetch_max(state.chunks.len(), Ordering::Relaxed);
            PEAK_BYTES.fetch_max(state.bytes, Ordering::Relaxed);
        }
        self.0.readable.notify_one();
        Ok(())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .producer_open = false;
        self.0.readable.notify_one();
    }
}
impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<StreamChunk> {
        loop {
            let ready = self.queue.readable.notified();
            {
                let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.cancelled || !state.consumer_open {
                    return None;
                }
                if let Some(chunk) = state.pop_batch() {
                    self.queue.writable.notify_all();
                    return Some(chunk);
                }
                if !state.producer_open {
                    return None;
                }
            }
            ready.await;
        }
    }
    pub(super) fn close(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.consumer_open = false;
        state.chunks.clear();
        state.bytes = 0;
        self.queue.writable.notify_all();
    }
    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<StreamChunk, ()> {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        let (chunk, bytes) = state.chunks.pop_front().ok_or(())?;
        state.bytes -= bytes;
        self.queue.writable.notify_all();
        Ok(chunk)
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.close();
        self.control.cancel();
        let state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        tracing::debug!(
            peak_events = state.peak_count,
            peak_payload_bytes = state.peak_bytes,
            "plugin provider bridge queue closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn delta(kind: &str, index: u64, text: &str) -> StreamChunk {
        serde_json::json!({"type":kind,"contentIndex":index,"delta":text})
    }

    #[tokio::test]
    async fn queued_unicode_deltas_share_a_snapshot_boundary_without_waiting_for_more() {
        for kind in ["text_delta", "thinking_delta"] {
            let (tx, mut rx) = channel(8, 4096, Arc::new(ProviderControlState::default()));
            for text in ["hello", "", " 🐈"] {
                tx.send(delta(kind, 2, text), 64).unwrap();
            }
            // Keep the sender open: a ready batch must not wait for another chunk.
            let chunk = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(chunk, delta(kind, 2, "hello 🐈"));
            {
                let state = rx.queue.state.lock().unwrap();
                assert!(state.chunks.is_empty());
                assert_eq!(state.bytes, 0);
            }
            drop(tx);
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn batching_preserves_snapshot_metadata_block_and_unknown_field_boundaries() {
        let mut authoritative = delta("text_delta", 0, "replace");
        authoritative["partial"] = serde_json::json!({"content":[]});
        let mut unfamiliar = delta("text_delta", 0, "future");
        unfamiliar["signature"] = serde_json::json!("must survive");
        for boundary in [
            authoritative,
            unfamiliar,
            delta("text_delta", 1, "next block"),
            delta("thinking_delta", 0, "thinking"),
            delta("toolcall_delta", 0, "{}"),
            serde_json::json!({"type":"usage","usage":{"input":3}}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":null}),
            serde_json::json!({"type":"text_delta","delta":"missing index"}),
        ] {
            let (tx, mut rx) = channel(4, 4096, Arc::new(ProviderControlState::default()));
            let first = delta("text_delta", 0, "a");
            let last = delta("text_delta", 0, "b");
            for chunk in [&first, &boundary, &last] {
                tx.send(chunk.clone(), 100).unwrap();
            }
            drop(tx);
            assert_eq!(rx.recv().await, Some(first));
            assert_eq!(rx.recv().await, Some(boundary));
            assert_eq!(rx.recv().await, Some(last));
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn batches_respect_the_byte_cap_and_cancellation() {
        let control = Arc::new(ProviderControlState::default());
        let (tx, mut rx) = channel(8, 65536, Arc::clone(&control));
        let text = "x".repeat(MAX_DELTA_BATCH_BYTES / 2);
        for _ in 0..3 {
            tx.send(delta("text_delta", 0, &text), text.len()).unwrap();
        }
        assert_eq!(
            rx.recv().await,
            Some(delta("text_delta", 0, &text.repeat(2)))
        );
        assert_eq!(rx.queue.state.lock().unwrap().bytes, text.len());
        control.cancel();
        assert!(
            rx.recv().await.is_none(),
            "cancelled queued content is not forwarded"
        );
        rx.close();
        assert_eq!(rx.queue.state.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn a_single_oversized_delta_is_delivered_unchanged() {
        let (tx, mut rx) = channel(2, 128, Arc::new(ProviderControlState::default()));
        let large = delta("text_delta", 0, &"x".repeat(MAX_DELTA_BATCH_BYTES + 1));
        tx.send(large.clone(), MAX_DELTA_BATCH_BYTES + 1).unwrap();
        assert_eq!(rx.recv().await, Some(large));
        let small = delta("text_delta", 0, "next");
        tx.send(small.clone(), 4).unwrap();
        assert_eq!(rx.recv().await, Some(small));
    }

    #[tokio::test]
    async fn capacity_one_dequeues_losslessly_and_wakes_blocking_producer() {
        let control = Arc::new(ProviderControlState::default());
        let (tx, mut rx) = channel(1, 4, control);
        tx.send(serde_json::json!({"i":0}), 10).unwrap(); // oversized occupies the budget exclusively
        let thread = std::thread::spawn(move || {
            tx.send(serde_json::json!({"i":1}), 1).unwrap();
        });
        assert_eq!(rx.recv().await.unwrap()["i"], 0);
        assert_eq!(rx.recv().await.unwrap()["i"], 1);
        thread.join().unwrap();
        assert!(rx.recv().await.is_none());
        let state = rx.queue.state.lock().unwrap();
        assert_eq!(state.peak_count, 1);
        assert_eq!(state.peak_bytes, 4);
    }

    #[test]
    fn cancellation_and_receiver_drop_release_capacity_waits() {
        for cancel in [false, true] {
            let control = Arc::new(ProviderControlState::default());
            let (tx, rx) = channel(1, 4, Arc::clone(&control));
            tx.send(serde_json::json!({}), 4).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                done_tx.send(tx.send(serde_json::json!({}), 1)).unwrap()
            });
            assert!(done_rx.recv_timeout(Duration::from_millis(10)).is_err());
            if cancel {
                control.cancel();
            } else {
                drop(rx);
            }
            assert!(done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err());
            thread.join().unwrap();
        }
    }
}
