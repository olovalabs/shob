//! Lossless SPSC assistant event stream. Ordinary events have a count limit and
//! an accounted payload budget; terminals use a separate slot. This bounds queued
//! payload, not allocator overhead, RSS, producer state, an in-flight event,
//! the final message, or consumer-held events.
//!
//! Rust API change: `push` must be awaited and `split` has been removed. Consume
//! with `next`, or call `result` to discard intermediates while awaiting completion.

use crate::types::{AssistantMessage, AssistantMessageEvent};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Maximum queued nonterminal events under the default limits.
pub const DEFAULT_EVENT_CAPACITY: usize = 64;
/// Default budget for accounted queued payload, excluding terminal events.
pub const DEFAULT_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

struct QueuedEvent {
    event: AssistantMessageEvent,
    _payload: OwnedSemaphorePermit,
}

#[cfg(not(test))]
type TerminalReceiver = oneshot::Receiver<AssistantMessageEvent>;

#[cfg(test)]
struct TerminalReceiver {
    inner: oneshot::Receiver<AssistantMessageEvent>,
    before_poll: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(test)]
impl std::future::Future for TerminalReceiver {
    type Output = Result<AssistantMessageEvent, oneshot::error::RecvError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if let Some(before_poll) = self.before_poll.take() {
            before_poll();
        }
        std::pin::Pin::new(&mut self.inner).poll(cx)
    }
}

pub struct AssistantMessageEventStream {
    rx: mpsc::Receiver<QueuedEvent>,
    terminal_rx: Option<TerminalReceiver>,
    result_rx: Option<oneshot::Receiver<AssistantMessage>>,
    cancellation: CancellationToken,
    done: bool,
}

impl AssistantMessageEventStream {
    /// Receive ordinary events in order, then the independently published terminal.
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        if self.done {
            return None;
        }
        if let Some(event) = self.rx.recv().await {
            return Some(event.event);
        }
        let terminal = self.terminal_rx.as_mut()?.await.ok();
        self.done = true;
        terminal
    }

    /// Discard intermediates so a producer blocked on capacity can finish.
    pub async fn result(mut self) -> Result<AssistantMessage, RecvError> {
        let mut result = self.result_rx.take().unwrap();
        loop {
            tokio::select! {
                biased;
                message = &mut result => return message.map_err(|_| RecvError),
                event = self.rx.recv() => {
                    if event.is_none() { return result.await.map_err(|_| RecvError); }
                }
            }
        }
    }
}

impl Drop for AssistantMessageEventStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvError;
impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "assistant-message event stream ended without a terminal Done/Error event"
        )
    }
}
impl std::error::Error for RecvError {}

pub struct AssistantMessageEventStreamProducer {
    tx: Option<mpsc::Sender<QueuedEvent>>,
    terminal_tx: Option<oneshot::Sender<AssistantMessageEvent>>,
    result_tx: Option<oneshot::Sender<AssistantMessage>>,
    payload: Arc<Semaphore>,
    payload_bytes: usize,
    cancellation: CancellationToken,
    committed: Option<Arc<AssistantMessage>>,
    done: bool,
}

impl AssistantMessageEventStreamProducer {
    /// The stream's child token. Use it for upstream reads and other producer work.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Last successfully queued snapshot; a cancelled capacity wait commits nothing.
    pub fn committed_message(&self) -> Option<&AssistantMessage> {
        self.committed.as_deref()
    }

    /// Checkpoint applied message metadata while retaining the last queued content.
    /// Cancellation during an enqueue prevents the frame's metadata from committing.
    pub fn commit_metadata(&mut self, message: &AssistantMessage) {
        if self.cancellation.is_cancelled() {
            return;
        }
        if let Some(previous) = self.committed.as_deref() {
            if previous.api == message.api
                && previous.provider == message.provider
                && previous.model == message.model
                && previous.response_model == message.response_model
                && previous.response_id == message.response_id
                && previous.usage == message.usage
                && previous.stop_reason == message.stop_reason
                && previous.deferred == message.deferred
                && previous.error_message == message.error_message
                && previous.raw_stop_reason == message.raw_stop_reason
                && previous.end_turn == message.end_turn
                && previous.timestamp == message.timestamp
            {
                return;
            }
        }
        self.committed = Some(Arc::new(AssistantMessage {
            role: message.role,
            content: self
                .committed
                .as_ref()
                .map(|previous| previous.content.clone())
                .unwrap_or_default(),
            api: message.api.clone(),
            provider: message.provider.clone(),
            model: message.model.clone(),
            response_model: message.response_model.clone(),
            response_id: message.response_id.clone(),
            usage: message.usage.clone(),
            stop_reason: message.stop_reason,
            deferred: message.deferred.clone(),
            error_message: message.error_message.clone(),
            raw_stop_reason: message.raw_stop_reason.clone(),
            end_turn: message.end_turn,
            timestamp: message.timestamp,
        }));
    }

    /// Await ordinary capacity. One oversized event exclusively consumes the byte
    /// budget. Terminal/result publication bypasses both limits; first terminal wins.
    /// False means cancellation or consumer closure. Subsequent terminal pushes are no-ops.
    pub async fn push(&mut self, mut event: AssistantMessageEvent) -> bool {
        if self.done {
            return true;
        }
        if event.is_terminal() {
            self.done = true;
            // Mapper state may include an event whose enqueue was cancelled. Only
            // successfully queued content belongs in the cancellation result.
            if self.cancellation.is_cancelled() {
                let source = event.partial();
                let mut error = self.committed.as_deref().cloned().unwrap_or_else(|| {
                    AssistantMessage::empty(
                        source.api.clone(),
                        source.provider.clone(),
                        source.model.clone(),
                        source.timestamp,
                    )
                });
                error.stop_reason = crate::types::StopReason::Aborted;
                error.error_message = Some(match &event {
                    AssistantMessageEvent::Error {
                        reason: crate::types::ErrorReason::Aborted,
                        error,
                    } => error
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Request aborted".into()),
                    _ => "Request aborted".into(),
                });
                event = AssistantMessageEvent::Error {
                    reason: crate::types::ErrorReason::Aborted,
                    error,
                };
            }
            let message = event.partial().clone();
            if let Some(tx) = self.result_tx.take() {
                let _ = tx.send(message);
            }
            let delivered = self.terminal_tx.take().unwrap().send(event).is_ok();
            // Closing the FIFO orders every queued event before the terminal,
            // even when publication races a consumer polling an empty queue.
            self.tx.take();
            return delivered;
        }
        let tx = self.tx.as_ref().unwrap();
        let weight = event
            .accounted_payload_bytes()
            .max(1)
            .min(self.payload_bytes) as u32;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => false,
            _ = tx.closed() => false,
            permit = self.payload.clone().acquire_many_owned(weight) => {
                let payload = permit.expect("payload semaphore remains open");
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => false,
                    permit = tx.reserve() => {
                        let Ok(permit) = permit else { return false; };
                        self.committed = Some(match &event {
                            AssistantMessageEvent::Start { partial }
                            | AssistantMessageEvent::TextStart { partial, .. }
                            | AssistantMessageEvent::TextDelta { partial, .. }
                            | AssistantMessageEvent::TextEnd { partial, .. }
                            | AssistantMessageEvent::ThinkingStart { partial, .. }
                            | AssistantMessageEvent::ThinkingDelta { partial, .. }
                            | AssistantMessageEvent::ThinkingEnd { partial, .. }
                            | AssistantMessageEvent::ToolCallStart { partial, .. }
                            | AssistantMessageEvent::ToolCallDelta { partial, .. }
                            | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial.clone(),
                            _ => unreachable!("terminals bypass ordinary queue"),
                        });
                        permit.send(QueuedEvent { event, _payload: payload });
                        true
                    }
                }
            }
        }
    }
    pub fn is_done(&self) -> bool {
        self.done
    }
    pub fn close(self) {
        drop(self);
    }
}

pub fn create_assistant_message_event_stream() -> (
    AssistantMessageEventStreamProducer,
    AssistantMessageEventStream,
) {
    create_assistant_message_event_stream_with_limits(
        DEFAULT_EVENT_CAPACITY,
        DEFAULT_PAYLOAD_BYTES,
        &CancellationToken::new(),
    )
}

/// Limits apply to ordinary events only. The stream owns a child of `parent`:
/// dropping the consumer stops the producer without cancelling its caller.
pub fn create_assistant_message_event_stream_with_limits(
    capacity: usize,
    payload_bytes: usize,
    parent: &CancellationToken,
) -> (
    AssistantMessageEventStreamProducer,
    AssistantMessageEventStream,
) {
    assert!(capacity > 0 && payload_bytes > 0 && payload_bytes <= u32::MAX as usize);
    let (tx, rx) = mpsc::channel(capacity);
    let (terminal_tx, terminal_rx) = oneshot::channel();
    #[cfg(test)]
    let terminal_rx = TerminalReceiver {
        inner: terminal_rx,
        before_poll: None,
    };
    let (result_tx, result_rx) = oneshot::channel();
    let cancellation = parent.child_token();
    (
        AssistantMessageEventStreamProducer {
            tx: Some(tx),
            terminal_tx: Some(terminal_tx),
            result_tx: Some(result_tx),
            payload: Arc::new(Semaphore::new(payload_bytes)),
            payload_bytes,
            cancellation: cancellation.clone(),
            committed: None,
            done: false,
        },
        AssistantMessageEventStream {
            rx,
            terminal_rx: Some(terminal_rx),
            result_rx: Some(result_rx),
            cancellation,
            done: false,
        },
    )
}

/// Default limits, with cancellation inherited from the caller.
pub fn create_assistant_message_event_stream_with_cancellation(
    parent: &CancellationToken,
) -> (
    AssistantMessageEventStreamProducer,
    AssistantMessageEventStream,
) {
    create_assistant_message_event_stream_with_limits(
        DEFAULT_EVENT_CAPACITY,
        DEFAULT_PAYLOAD_BYTES,
        parent,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, DoneReason};
    use std::sync::Arc;

    fn empty_partial() -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage::empty(Api::Faux, "faux", "faux", 0))
    }

    #[tokio::test]
    async fn drain_deltas_then_done() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();

        let partial = empty_partial();
        prod.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hi".into(),
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "hi".into(),
            partial: partial.clone(),
        })
        .await;

        let mut final_msg = (*partial).clone();
        final_msg.stop_reason = crate::types::StopReason::Stop;
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: final_msg.clone(),
        })
        .await;

        let mut tags = Vec::new();
        while let Some(ev) = stream.next().await {
            tags.push(ev.type_tag());
        }
        assert_eq!(
            tags,
            vec!["start", "text_start", "text_delta", "text_end", "done"]
        );

        let result = stream.result().await.unwrap();
        assert!(matches!(result.stop_reason, crate::types::StopReason::Stop));
    }

    #[tokio::test]
    async fn error_path_resolves_to_error_message() {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let err_msg = AssistantMessage::terminal(
            Api::Faux,
            "faux",
            "faux",
            crate::types::StopReason::Aborted,
            "cancelled",
            0,
        );
        prod.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorReason::Aborted,
            error: err_msg.clone(),
        })
        .await;
        let result = stream.result().await.unwrap();
        assert!(matches!(
            result.stop_reason,
            crate::types::StopReason::Aborted
        ));
        assert_eq!(result.error_message.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn push_after_terminal_is_noop() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();
        let msg = AssistantMessage::terminal(
            Api::Faux,
            "faux",
            "faux",
            crate::types::StopReason::Stop,
            "",
            0,
        );
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: msg,
        })
        .await;
        // Post-terminal push should not deliver.
        prod.push(AssistantMessageEvent::Start {
            partial: empty_partial(),
        })
        .await;

        let first = stream.next().await.unwrap();
        assert!(matches!(first, AssistantMessageEvent::Done { .. }));
        // Stream ends after the terminal event (producer won't send more that land).
        assert!(stream.next().await.is_none());
    }
    fn text_event(text: &str) -> AssistantMessageEvent {
        let mut message = (*empty_partial()).clone();
        message.content.push(crate::types::Content::text(text));
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text.into(),
            partial: Arc::new(message),
        }
    }

    fn done_event() -> AssistantMessageEvent {
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: (*empty_partial()).clone(),
        }
    }

    #[tokio::test]
    async fn capacity_one_waits_and_resumes_without_losing_order() {
        let parent = CancellationToken::new();
        let (mut prod, mut stream) =
            create_assistant_message_event_stream_with_limits(1, DEFAULT_PAYLOAD_BYTES, &parent);
        assert!(prod.push(text_event("first")).await);
        let mut blocked = Box::pin(prod.push(text_event("second")));
        assert!(futures::poll!(&mut blocked).is_pending());
        assert_eq!(stream.next().await.unwrap(), text_event("first"));
        assert!(blocked.await);
        assert!(prod.push(done_event()).await);
        assert_eq!(stream.next().await.unwrap(), text_event("second"));
        assert!(stream.next().await.unwrap().is_terminal());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn byte_budget_and_oversized_event_are_exclusive() {
        let parent = CancellationToken::new();
        let event = text_event("small");
        let bytes = event.accounted_payload_bytes();
        let (mut prod, mut stream) =
            create_assistant_message_event_stream_with_limits(8, bytes, &parent);
        assert!(prod.push(event).await);
        let oversized = text_event(&"x".repeat(bytes * 2));
        assert!(oversized.accounted_payload_bytes() > bytes);
        let mut blocked = Box::pin(prod.push(oversized.clone()));
        assert!(futures::poll!(&mut blocked).is_pending());
        stream.next().await.unwrap();
        assert!(blocked.await);
        let mut following = Box::pin(prod.push(text_event("after")));
        assert!(futures::poll!(&mut following).is_pending());
        assert_eq!(stream.next().await.unwrap(), oversized);
        assert!(following.await);
    }

    #[tokio::test]
    async fn result_only_drains_a_saturated_producer() {
        let (mut prod, stream) =
            create_assistant_message_event_stream_with_limits(1, 1, &CancellationToken::new());
        let producer = tokio::spawn(async move {
            for index in 0..100 {
                assert!(prod.push(text_event(&index.to_string())).await);
            }
            assert!(prod.push(done_event()).await);
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.result())
            .await
            .unwrap()
            .unwrap();
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn slow_consumer_receives_every_event() {
        let (mut prod, mut stream) = create_assistant_message_event_stream_with_limits(
            1,
            DEFAULT_PAYLOAD_BYTES,
            &CancellationToken::new(),
        );
        let producer = tokio::spawn(async move {
            for index in 0..100 {
                assert!(prod.push(text_event(&index.to_string())).await);
            }
            prod.push(done_event()).await;
        });
        for index in 0..100 {
            tokio::task::yield_now().await;
            assert_eq!(stream.next().await.unwrap(), text_event(&index.to_string()));
        }
        assert!(stream.next().await.unwrap().is_terminal());
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn terminal_and_result_publish_while_ordinary_capacity_is_full() {
        let (mut prod, mut stream) =
            create_assistant_message_event_stream_with_limits(1, 1, &CancellationToken::new());
        prod.push(text_event("queued")).await;
        let mut terminal = Box::pin(prod.push(done_event()));
        assert!(matches!(
            futures::poll!(&mut terminal),
            std::task::Poll::Ready(true)
        ));
        drop(terminal);
        assert!(stream.result_rx.as_mut().unwrap().try_recv().is_ok());
        assert_eq!(stream.next().await.unwrap(), text_event("queued"));
        assert!(stream.next().await.unwrap().is_terminal());
        assert!(prod.push(text_event("ignored")).await);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn cancellation_releases_waiters_and_discards_uncommitted_snapshot() {
        let parent = CancellationToken::new();
        let (mut prod, mut stream) =
            create_assistant_message_event_stream_with_limits(1, DEFAULT_PAYLOAD_BYTES, &parent);
        prod.push(text_event("committed")).await;
        let mut blocked = Box::pin(prod.push(text_event("must not commit")));
        assert!(futures::poll!(&mut blocked).is_pending());
        parent.cancel();
        assert!(!blocked.await);
        assert_eq!(
            prod.committed_message().unwrap().content,
            text_event("committed").partial().content
        );
        prod.push(done_event()).await;
        assert_eq!(stream.next().await.unwrap(), text_event("committed"));
        let terminal = stream.next().await.unwrap();
        assert!(matches!(
            terminal,
            AssistantMessageEvent::Error {
                reason: crate::types::ErrorReason::Aborted,
                ..
            }
        ));
        assert_eq!(
            terminal.partial().content,
            text_event("committed").partial().content
        );
        assert_eq!(
            stream.result().await.unwrap().content,
            terminal.partial().content
        );
    }

    #[tokio::test]
    async fn drop_consumer_cancels_only_child_and_releases_waiter() {
        let parent = CancellationToken::new();
        let (mut prod, stream) = create_assistant_message_event_stream_with_limits(1, 1, &parent);
        let child = prod.cancellation_token();
        prod.push(text_event("queued")).await;
        let mut blocked = Box::pin(prod.push(text_event("blocked")));
        assert!(futures::poll!(&mut blocked).is_pending());
        drop(stream);
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(!blocked.await);
    }

    #[tokio::test]
    async fn cancellation_before_first_event_keeps_result_empty() {
        let parent = CancellationToken::new();
        let (mut prod, stream) = create_assistant_message_event_stream_with_limits(1, 1, &parent);
        parent.cancel();
        assert!(!prod.push(text_event("never queued")).await);
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: text_event("never queued").partial().clone(),
        })
        .await;
        let result = stream.result().await.unwrap();
        assert!(result.content.is_empty());
        assert_eq!(result.stop_reason, crate::types::StopReason::Aborted);
    }

    #[test]
    fn payload_accounting_walks_metadata_and_json_without_serializing() {
        let event = text_event("hello");
        let mut message = event.partial().clone();
        message.response_id = Some("r".repeat(100));
        message.content.push(crate::types::Content::tool_call(
            "id",
            "tool",
            serde_json::json!({"key": "v".repeat(1000)}),
        ));
        let bigger = AssistantMessageEvent::Start {
            partial: Arc::new(message),
        };
        assert!(bigger.accounted_payload_bytes() >= event.accounted_payload_bytes() + 1000);
    }

    #[tokio::test]
    async fn metadata_checkpoint_keeps_only_committed_content() {
        let parent = CancellationToken::new();
        let (mut prod, stream) = create_assistant_message_event_stream_with_limits(1, 1, &parent);
        prod.push(text_event("committed")).await;
        let mut pending = text_event("buffered but unqueued").partial().clone();
        pending.usage.input = 42;
        prod.commit_metadata(&pending);
        parent.cancel();
        prod.push(done_event()).await;
        let result = stream.result().await.unwrap();
        assert_eq!(result.content, text_event("committed").partial().content);
        assert_eq!(result.usage.input, 42);
    }

    #[tokio::test]
    async fn first_terminal_wins_result_and_event() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();
        prod.push(done_event()).await;
        prod.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorReason::Error,
            error: AssistantMessage::terminal(
                Api::Faux,
                "faux",
                "faux",
                crate::types::StopReason::Error,
                "late error",
                0,
            ),
        })
        .await;
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert!(stream.result().await.unwrap().error_message.is_none());
    }
    #[tokio::test]
    async fn terminal_publication_between_channel_polls_cannot_overtake_an_event() {
        use std::sync::Mutex;
        use std::task::Poll;

        let (producer, mut stream) = create_assistant_message_event_stream();
        let publication = Arc::new(Mutex::new(Some(producer)));
        let publish = |publication: &Mutex<Option<AssistantMessageEventStreamProducer>>| {
            if let Some(mut producer) = publication.lock().unwrap().take() {
                futures::executor::block_on(async {
                    assert!(producer.push(text_event("last ordinary event")).await);
                    assert!(producer.push(done_event()).await);
                });
            }
        };
        let during_terminal_poll = publication.clone();
        stream.terminal_rx.as_mut().unwrap().before_poll = Some(Box::new(move || {
            // Reproduce publication after the FIFO was polled empty, but
            // before the old select polled the independent terminal receiver.
            publish(&during_terminal_poll);
        }));
        let mut next = Box::pin(stream.next());
        let first_poll = futures::poll!(&mut next);
        // A consumer draining only the FIFO never polls the terminal early.
        // Publish now if that receive correctly remained pending.
        publish(&publication);
        let event = match first_poll {
            Poll::Ready(event) => event,
            Poll::Pending => next.as_mut().await,
        };
        drop(next);
        assert_eq!(event, Some(text_event("last ordinary event")));
        assert!(stream.next().await.unwrap().is_terminal());
        assert!(stream.next().await.is_none());
        assert!(stream.result().await.is_ok());
    }

    #[tokio::test]
    async fn cancelling_next_while_producer_closes_keeps_the_terminal() {
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        // Producer fields can drop on another thread between ordinary-channel
        // closure and terminal publication/closure. Hold that interval open.
        drop(producer.tx.take());
        let mut pending = Box::pin(stream.next());
        assert!(futures::poll!(&mut pending).is_pending());
        drop(pending);
        assert!(producer.push(done_event()).await);
        assert!(stream.next().await.unwrap().is_terminal());
        assert!(stream.next().await.is_none());
        assert!(stream.result().await.is_ok());
    }
}
