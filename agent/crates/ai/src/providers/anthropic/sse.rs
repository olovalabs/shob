//! Mirrors `packages/ai/src/api/anthropic-messages.ts` — the SSE wire decoder
//! (`decodeSseLine` / `flushSseEvent` / `iterateSseMessages` /
//! `iterateAnthropicEvents`) ported to a pull-based state machine over a
//! `reqwest::Response::bytes_stream()`.
//!
//! The TS decoder is line-buffered: an empty line flushes the pending event;
//! `:` lines are comments; `event:`/`data:` set fields; multiple `data:` lines
//! are joined with `\n`. The Rust port reproduces that exactly, plus the
//! per-message validation: an `error` SSE event throws; a non-anthropic event
//! type is skipped; `message_start` without a `message_stop` throws. The
//! `RawMessageStreamEvent` union is modeled as a permissive `serde_json::Value`
//! (with a `type` discriminator) so the mapper can dispatch by event name
//! without binding to the Anthropic SDK's TS types.

use crate::error::AiError;
use crate::providers::anthropic::json_parse::parse_json_with_repair;
use bytes::Bytes;
use futures::StreamExt;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// The `ANTHROPIC_MESSAGE_EVENTS` set — event types the mapper consumes. Any
/// other event name is dropped (its `data` is never parsed). Mirrors the
/// `ANTHROPIC_MESSAGE_EVENTS` Set in the TS source.
pub const ANTHROPIC_MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// A decoded SSE frame. `event` is `None` when the server emitted no `event:`
/// line (Anthropic always sends one, but the spec allows nameless events).
#[derive(Debug, Clone)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    /// The raw `data:` line payloads (before `\n`-join) — preserved so error
    /// messages can mirror the TS `raw.join("\\n")` diagnostic.
    pub raw: Vec<String>,
}

/// Accumulator state for the line-buffered decoder. Mirrors `SseDecoderState`.
#[derive(Debug, Default)]
pub struct SseDecoderState {
    started: bool,
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoderState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one complete line. Returns `Some(event)` when the line is empty
    /// (the flush trigger) and a pending event exists. Mirrors `decodeSseLine`.
    pub fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        // SSE's UTF-8 decoding strips exactly one leading BOM. Complete lines
        // reach this decoder only after fragmented code points are reassembled.
        let line = if std::mem::replace(&mut self.started, true) {
            line
        } else {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        };
        if line.is_empty() {
            return self.flush();
        }

        self.raw.push(line.to_string());
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.find(':') {
            None => (line.to_string(), String::new()),
            Some(idx) => {
                let f = line[..idx].to_string();
                let mut v = line[idx + 1..].to_string();
                if let Some(stripped) = v.strip_prefix(' ') {
                    v = stripped.to_string();
                }
                (f, v)
            }
        };

        if field == "event" {
            self.event = Some(value);
        } else if field == "data" {
            self.data.push(value);
        }

        None
    }

    /// Flush the pending event, if any. Mirrors `flushSseEvent`.
    pub fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            // Comment-only frames (for example keepalives) do not produce an
            // event, but their raw lines must not accumulate for the lifetime
            // of a long-lived stream.
            self.raw.clear();
            return None;
        }
        let event = ServerSentEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
            raw: std::mem::take(&mut self.raw),
        };
        self.data.clear();
        Some(event)
    }
}

/// Find the next `\r` or `\n` boundary, returning the byte index (or `None` if
/// neither is present). Mirrors `nextLineBreakIndex`.
fn next_line_break_index(text: &str) -> Option<usize> {
    memchr::memchr2(b'\r', b'\n', text.as_bytes())
}

/// Split the first complete line off `text`, consuming a trailing `\r\n` or
/// single `\r`/`\n`. A trailing `\r` completes a line immediately; the stream
/// suppresses a possible leading `\n` in the next HTTP chunk.
fn consume_line(text: &str) -> Option<(&str, &str)> {
    let idx = next_line_break_index(text)?;
    let mut next = idx + 1;
    if text.as_bytes().get(idx) == Some(&b'\r') && text.as_bytes().get(next) == Some(&b'\n') {
        next += 1;
    }
    Some((&text[..idx], &text[next..]))
}

/// Append one HTTP body chunk to the line buffer using TextDecoder-like UTF-8
/// handling.  A partial code point is retained for the next chunk, while an
/// actually malformed sequence is replaced immediately so it cannot pin all
/// following bytes in `utf8_buffer` until the response ends.
fn append_utf8_chunk(buffer: &mut String, utf8_buffer: &mut Vec<u8>, chunk: &[u8]) {
    if utf8_buffer.is_empty() {
        if let Ok(text) = std::str::from_utf8(chunk) {
            buffer.push_str(text);
            return;
        }
    }
    utf8_buffer.extend_from_slice(chunk);

    loop {
        match std::str::from_utf8(utf8_buffer) {
            Ok(text) => {
                buffer.push_str(text);
                utf8_buffer.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // `valid_up_to` always ends at a UTF-8 code-point boundary.
                    let text = std::str::from_utf8(&utf8_buffer[..valid])
                        .expect("valid_up_to must identify valid UTF-8");
                    buffer.push_str(text);
                    utf8_buffer.drain(..valid);
                    continue;
                }

                if let Some(error_len) = error.error_len() {
                    // Match the replacement behavior of the Web TextDecoder
                    // used by the reference implementation.  Consume only the
                    // malformed sequence; valid bytes after it remain usable.
                    buffer.push('\u{FFFD}');
                    utf8_buffer.drain(..error_len);
                    continue;
                }

                // No error length means the suffix is an incomplete code point.
                // Keep it until the next HTTP chunk (or the EOF lossily-flushed
                // tail below).
                break;
            }
        }
    }
}

/// Pull-based SSE event stream over a `reqwest` response body. Mirrors the
/// async iterator `iterateSseMessages` — call `next_event` to pull one
/// `ServerSentEvent` at a time so the mapper can push wire events as they
/// arrive (true time-to-first-token) instead of buffering the whole body.
///
/// Honors `signal` between chunk reads: a cancellation surfaces as
/// `AiError::Abort`. Stream end surfaces the trailing flush (if any) then
/// `Ok(None)`.
pub struct SseEventStream {
    bytes_stream: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    state: SseDecoderState,
    buffer: String,
    cursor: usize,
    utf8_buffer: Vec<u8>,
    skip_lf: bool,
    signal: CancellationToken,
    idle_timeout: Option<std::time::Duration>,
    last_bytes_at: tokio::time::Instant,
    done: bool,
    diagnostics_started: Option<Instant>,
    diagnostics_first_event_ns: Option<u64>,
    diagnostics_event_count: u64,
}

impl SseEventStream {
    pub fn new(response: reqwest::Response, signal: CancellationToken) -> Self {
        Self {
            bytes_stream: response.bytes_stream().boxed(),
            state: SseDecoderState::new(),
            buffer: String::new(),
            cursor: 0,
            utf8_buffer: Vec::new(),
            skip_lf: false,
            signal,
            idle_timeout: Some(crate::provider::DEFAULT_STREAM_IDLE_TIMEOUT),
            last_bytes_at: tokio::time::Instant::now(),
            done: false,
            diagnostics_started: None,
            diagnostics_first_event_ns: None,
            diagnostics_event_count: 0,
        }
    }

    /// Set the maximum silence between body chunks. `None` or zero disables
    /// the watchdog. SSE comments and partial frames count as activity.
    pub fn with_idle_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.idle_timeout = timeout.filter(|value| !value.is_zero());
        self
    }

    /// Enable optional timing of the first decoded SSE event. Disabled by
    /// default so the normal provider path does not add timing work.
    pub fn with_diagnostics(mut self, enabled: bool) -> Self {
        if enabled {
            self.diagnostics_started = Some(Instant::now());
        }
        self
    }

    /// Return the elapsed time until the first decoded SSE event.
    pub fn diagnostics_first_event_ns(&self) -> Option<u64> {
        self.diagnostics_first_event_ns
    }

    /// Return the number of decoded SSE frames handed to the mapper while
    /// diagnostics were enabled. This counts protocol frames, not mapped
    /// `AssistantMessageEvent`s; one Anthropic frame may update state without
    /// emitting an agent event.
    pub fn diagnostics_event_count(&self) -> u64 {
        self.diagnostics_event_count
    }

    fn record_diagnostic_event(&mut self, event: ServerSentEvent) -> ServerSentEvent {
        if self.diagnostics_started.is_some() || self.diagnostics_first_event_ns.is_some() {
            self.diagnostics_event_count += 1;
        }
        if self.diagnostics_first_event_ns.is_none() {
            if let Some(started) = self.diagnostics_started.take() {
                self.diagnostics_first_event_ns =
                    Some(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            }
        }
        event
    }

    fn record_diagnostic_option(
        &mut self,
        event: Option<ServerSentEvent>,
    ) -> Option<ServerSentEvent> {
        event.map(|event| self.record_diagnostic_event(event))
    }

    /// Return `None` when the mapper's pending delta must be flushed. All
    /// partial input is kept in the stream, so the next read can resume it.
    pub(crate) async fn next_event_before(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Option<Result<Option<ServerSentEvent>, AiError>> {
        match deadline {
            Some(deadline) => tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => None,
                next = self.next_event() => Some(next),
            },
            None => Some(self.next_event().await),
        }
    }

    /// Pull the next SSE event, or `Ok(None)` when the body is exhausted (after
    /// the trailing flush). Mirrors one iteration of `iterateSseMessages`.
    pub async fn next_event(&mut self) -> Result<Option<ServerSentEvent>, AiError> {
        loop {
            // One body chunk may contain several complete events. Cancellation
            // must win before dispatching buffered events as well as body reads.
            if self.signal.is_cancelled() {
                return Err(AiError::Abort {
                    message: "Request was aborted".to_string(),
                });
            }
            // Retain the consumed prefix while buffered events remain. Moving
            // the unread suffix after each event makes large bursts quadratic.
            while self.cursor < self.buffer.len() {
                if std::mem::take(&mut self.skip_lf) && self.buffer[self.cursor..].starts_with('\n')
                {
                    self.cursor += 1;
                }
                let slice = &self.buffer[self.cursor..];
                let Some((line, rest)) = consume_line(slice) else {
                    break;
                };
                self.skip_lf = rest.is_empty() && slice.ends_with('\r');
                self.cursor += slice.len() - rest.len();
                if let Some(event) = self.state.decode_line(line) {
                    return Ok(Some(self.record_diagnostic_event(event)));
                }
            }
            // Reclaim the prefix once per body read, reusing the allocation and
            // preserving any partial line across a cancelled read or deadline.
            if self.cursor != 0 {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
            }

            if self.done {
                if !self.utf8_buffer.is_empty() {
                    self.buffer
                        .push_str(&String::from_utf8_lossy(&self.utf8_buffer));
                    self.utf8_buffer.clear();
                }
                if !self.buffer.is_empty() {
                    let event = self.state.decode_line(&self.buffer);
                    self.buffer.clear();
                    if let Some(event) = event {
                        return Ok(Some(self.record_diagnostic_event(event)));
                    }
                }
                let flushed = self.state.flush();
                return Ok(self.record_diagnostic_option(flushed));
            }

            // Cancellation must race the body read itself. Checking the token
            // only before `.next().await` leaves the task stuck forever when a
            // server keeps the SSE connection open without sending another
            // chunk (and makes Ctrl+C/Esc appear to freeze the TUI).
            let next = tokio::select! {
                biased;
                _ = self.signal.cancelled() => {
                    return Err(AiError::Abort {
                        message: "Request was aborted".to_string(),
                    });
                }
                next = self.bytes_stream.next() => next,
                _ = async {
                    match self.idle_timeout.and_then(|timeout| self.last_bytes_at.checked_add(timeout)) {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return Err(AiError::Http {
                        status: Some(408),
                        message: "stream idle timeout: no response bytes received; retry the request or increase --stream-idle-timeout".into(),
                    });
                }
            };

            match next {
                None => {
                    self.done = true;
                    continue;
                }
                Some(Err(e)) => {
                    // A transport error can race cancellation (for example a
                    // peer closes the socket as the caller aborts). Preserve
                    // the cancellation contract instead of surfacing a
                    // misleading SSE/body failure in that case.
                    if self.signal.is_cancelled() {
                        return Err(AiError::Abort {
                            message: "Request was aborted".to_string(),
                        });
                    }
                    let mut source = String::new();
                    let mut current: &(dyn std::error::Error + 'static) = &e;
                    while let Some(next) = current.source() {
                        if !source.is_empty() {
                            source.push_str("; ");
                        }
                        source.push_str(&next.to_string());
                        current = next;
                    }
                    let detail = if source.is_empty() {
                        e.to_string()
                    } else {
                        format!("{} (caused by: {source})", e)
                    };
                    return Err(AiError::Sse {
                        message: format!("error reading sse body: {detail}"),
                    });
                }
                Some(Ok(chunk)) => {
                    if !chunk.is_empty() {
                        self.last_bytes_at = tokio::time::Instant::now();
                    }
                    append_utf8_chunk(&mut self.buffer, &mut self.utf8_buffer, &chunk);
                }
            }
        }
    }
}

/// Pull `ServerSentEvent`s from a streaming `reqwest` response body, honoring
/// `signal` between chunk reads. Mirrors `iterateSseMessages`. Returns
/// `AiError::Abort` when the token fires.
///
/// This is the collecting wrapper retained for tests; the streaming provider
/// path uses [`SseEventStream::next_event`] directly so events map as they
/// arrive.
pub async fn iterate_sse_messages(
    response: reqwest::Response,
    signal: &CancellationToken,
) -> Result<Vec<ServerSentEvent>, AiError> {
    let mut stream = SseEventStream::new(response, signal.clone());
    let mut out = Vec::new();
    while let Some(event) = stream.next_event().await? {
        out.push(event);
    }
    Ok(out)
}

/// Parse an SSE `data` payload into a typed-ish `AnthropicEvent`. Mirrors the
/// per-event `parseJsonWithRepair<RawMessageStreamEvent>(sse.data)` call in
/// `iterateAnthropicEvents`, plus its validation:
/// - `event: "error"` → `AiError::Provider { code: "sse_error", message: data }`.
/// - non-`ANTHROPIC_MESSAGE_EVENTS` → `AnthropicEvent::Skipped`.
/// - JSON parse failure → `AiError::Sse` with the TS-style diagnostic.
pub fn parse_anthropic_event(sse: &ServerSentEvent) -> Result<AnthropicEvent, AiError> {
    if sse.event.as_deref() == Some("error") {
        return Err(AiError::Provider {
            code: "sse_error".to_string(),
            message: sse.data.clone(),
        });
    }

    let event_name = match &sse.event {
        Some(name) if ANTHROPIC_MESSAGE_EVENTS.contains(&name.as_str()) => name.clone(),
        _ => return Ok(AnthropicEvent::Skipped),
    };

    let value: serde_json::Value = parse_json_with_repair(&sse.data).map_err(|e| AiError::Sse {
        message: format!(
            "Could not parse Anthropic SSE event {}: {}; data={}; raw={}",
            event_name,
            e,
            sse.data,
            sse.raw.join("\\n"),
        ),
    })?;

    Ok(AnthropicEvent::Message {
        event_type: event_name,
        payload: value,
    })
}

/// A decoded Anthropic SSE event. `Skipped` covers comments / ping / unknown
/// event types so the mapper can ignore them without a `continue` ripple.
#[derive(Debug, Clone)]
pub enum AnthropicEvent {
    /// A `message_start` / `message_delta` / `message_stop` / `content_block_*`
    /// event whose `data` parsed to a JSON object carrying a `type` field equal
    /// to the SSE `event:` name (Anthropic's convention).
    Message {
        event_type: String,
        payload: serde_json::Value,
    },
    /// A non-message SSE event (ping, comment, extension). Dropped by the mapper.
    Skipped,
}

impl AnthropicEvent {
    /// The `type` discriminator from the payload (equal to the SSE event name
    /// in practice). Convenience for the mapper's match.
    pub fn event_type(&self) -> Option<&str> {
        match self {
            AnthropicEvent::Message { event_type, .. } => Some(event_type),
            AnthropicEvent::Skipped => None,
        }
    }

    pub fn payload(&self) -> Option<&serde_json::Value> {
        match self {
            AnthropicEvent::Message { payload, .. } => Some(payload),
            AnthropicEvent::Skipped => None,
        }
    }
}

/// Decode a full SSE response into the message events the mapper consumes,
/// applying the `message_start`-without-`message_stop` invariant. Mirrors
/// `iterateAnthropicEvents` over `iterateSseMessages`.
pub async fn iterate_anthropic_events(
    response: reqwest::Response,
    signal: &CancellationToken,
) -> Result<Vec<AnthropicEvent>, AiError> {
    let frames = iterate_sse_messages(response, signal).await?;
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    let mut out = Vec::with_capacity(frames.len());

    for frame in frames {
        let event = parse_anthropic_event(&frame)?;
        match &event {
            AnthropicEvent::Message { event_type, .. } => {
                if event_type == "message_start" {
                    saw_message_start = true;
                } else if event_type == "message_stop" {
                    saw_message_stop = true;
                }
                out.push(event);
            }
            AnthropicEvent::Skipped => {}
        }
    }

    if saw_message_start && !saw_message_stop {
        return Err(AiError::Sse {
            message: "Anthropic stream ended before message_stop".to_string(),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_test_stream() -> (tokio::sync::mpsc::Sender<Bytes>, SseEventStream) {
        let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(8);
        let chunks = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|chunk| (Ok(chunk), receiver))
        })
        .boxed();
        let stream = SseEventStream {
            bytes_stream: chunks,
            state: SseDecoderState::new(),
            buffer: String::new(),
            cursor: 0,
            utf8_buffer: Vec::new(),
            skip_lf: false,
            signal: CancellationToken::new(),
            idle_timeout: Some(std::time::Duration::from_secs(5)),
            last_bytes_at: tokio::time::Instant::now(),
            done: false,
            diagnostics_started: None,
            diagnostics_first_event_ns: None,
            diagnostics_event_count: 0,
        };
        (sender, stream)
    }

    #[tokio::test]
    async fn diagnostics_count_each_decoded_frame_once() {
        let (sender, stream) = idle_test_stream();
        let mut stream = stream.with_diagnostics(true);
        sender
            .send(Bytes::from_static(
                b"event: first\ndata: 1\n\nevent: second\ndata: 2\n\n",
            ))
            .await
            .unwrap();
        drop(sender);

        assert_eq!(
            stream.next_event().await.unwrap().unwrap().event.as_deref(),
            Some("first")
        );
        assert_eq!(
            stream.next_event().await.unwrap().unwrap().event.as_deref(),
            Some("second")
        );
        assert!(stream.next_event().await.unwrap().is_none());
        assert_eq!(stream.diagnostics_event_count(), 2);
    }
    #[tokio::test(start_paused = true)]
    async fn leading_bom_does_not_drop_the_first_event_when_split_across_chunks() {
        for split in 0..=3 {
            let (sender, mut stream) = idle_test_stream();
            let bom = b"\xef\xbb\xbf";
            sender
                .send(Bytes::copy_from_slice(&bom[..split]))
                .await
                .unwrap();
            sender
                .send(Bytes::copy_from_slice(&bom[split..]))
                .await
                .unwrap();
            sender
                .send(Bytes::from_static(b"data: first\n\ndata: second\n\n"))
                .await
                .unwrap();
            drop(sender);
            assert_eq!(
                stream.next_event().await.unwrap().unwrap().data,
                "first",
                "split={split}"
            );
            assert_eq!(stream.next_event().await.unwrap().unwrap().data, "second");
            assert!(stream.next_event().await.unwrap().is_none());
        }
    }

    #[test]
    fn only_one_bom_at_the_start_of_the_stream_is_stripped() {
        let events = decode_all("\u{feff}event: message\ndata: \u{feff}payload\n\n\u{feff}data: ignored\n\ndata: later\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "\u{feff}payload");
        assert_eq!(events[1].data, "later");
        for prefix in ["\n", ": comment\n", "\u{feff}"] {
            assert!(decode_all(&format!("{prefix}\u{feff}data: ignored\n\n")).is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cr_delimited_frames_are_dispatched_before_more_bytes_or_eof() {
        let (sender, mut stream) = idle_test_stream();
        sender
            .send(Bytes::from_static(b"data: first\r\r"))
            .await
            .unwrap();
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "first");
        sender
            .send(Bytes::from_static(b"data: second\r\r"))
            .await
            .unwrap();
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "second");
    }

    #[tokio::test(start_paused = true)]
    async fn all_line_endings_preserve_multiline_frames_when_every_byte_is_a_chunk() {
        for ending in ["\r", "\r\n", "\n"] {
            let (sender, mut stream) = idle_test_stream();
            let data = format!("event: message{ending}data: one{ending}data: two{ending}{ending}data: later{ending}{ending}");
            let writer = tokio::spawn(async move {
                for byte in data.bytes() {
                    sender.send(Bytes::from(vec![byte])).await.unwrap();
                }
            });
            let first = stream.next_event().await.unwrap().unwrap();
            assert_eq!(first.event.as_deref(), Some("message"));
            assert_eq!(first.data, "one\ntwo", "ending={ending:?}");
            assert_eq!(stream.next_event().await.unwrap().unwrap().data, "later");
            assert!(stream.next_event().await.unwrap().is_none());
            writer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn buffered_burst_keeps_events_in_order_across_chunk_boundaries() {
        use std::fmt::Write as _;

        let (sender, mut stream) = idle_test_stream();
        let mut burst = String::new();
        for index in 0..1024 {
            write!(burst, "data: {index}\r\n\r\n").unwrap();
        }
        burst.pop();
        sender.send(Bytes::from(burst)).await.unwrap();
        sender
            .send(Bytes::from_static(b"\ndata: last"))
            .await
            .unwrap();
        drop(sender);

        for index in 0..1024 {
            let event = stream.next_event().await.unwrap().unwrap();
            assert_eq!(event.data, index.to_string());
        }
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "last");
        assert!(stream.next_event().await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn delta_deadline_preserves_partial_sse_frame() {
        let (sender, mut stream) = idle_test_stream();
        sender.send(Bytes::from_static(b"data: par")).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(8);
        assert!(stream.next_event_before(Some(deadline)).await.is_none());
        sender.send(Bytes::from_static(b"tial\n\n")).await.unwrap();
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "partial");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_interrupts_a_stream_stalled_mid_response() {
        let (sender, mut stream) = idle_test_stream();
        sender
            .send(Bytes::from_static(b"data: first\n\n"))
            .await
            .unwrap();
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "first");
        // A partial frame is also network activity, but cannot mask a later stall.
        sender
            .send(Bytes::from_static(b"data: partial"))
            .await
            .unwrap();
        let start = tokio::time::Instant::now();
        let error = stream.next_event().await.unwrap_err();
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(5));
        assert!(error.to_string().contains("stream idle timeout"));
        assert!(error.is_retryable());
        assert!(!error.is_abort());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_resets_on_comments_and_partial_frames() {
        let (sender, mut stream) = idle_test_stream();
        let writer = tokio::spawn(async move {
            for chunk in [": keepalive\n\n", "data: ", "still ", "working\n\n"] {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                sender.send(Bytes::from(chunk)).await.unwrap();
            }
        });
        let start = tokio::time::Instant::now();
        assert_eq!(
            stream.next_event().await.unwrap().unwrap().data,
            "still working"
        );
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(12));
        writer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_idle_timeout_allows_long_silence() {
        let (sender, stream) = idle_test_stream();
        let mut stream = stream.with_idle_timeout(Some(std::time::Duration::ZERO));
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            sender
                .send(Bytes::from_static(b"data: late\n\n"))
                .await
                .unwrap();
        });
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "late");
        writer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wins_over_events_already_buffered_from_the_same_chunk() {
        let (sender, mut stream) = idle_test_stream();
        sender
            .send(Bytes::from_static(b"data: first\n\ndata: second\n\n"))
            .await
            .unwrap();
        assert_eq!(stream.next_event().await.unwrap().unwrap().data, "first");
        stream.signal.cancel();
        assert!(matches!(
            stream.next_event().await,
            Err(AiError::Abort { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wins_over_an_expired_idle_deadline() {
        let (_sender, mut stream) = idle_test_stream();
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        stream.signal.cancel();
        assert!(matches!(
            stream.next_event().await,
            Err(AiError::Abort { .. })
        ));
    }

    fn decode_all(input: &str) -> Vec<ServerSentEvent> {
        let mut state = SseDecoderState::new();
        let mut out = Vec::new();
        for line in input.split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if let Some(event) = state.decode_line(trimmed) {
                out.push(event);
            }
        }
        // Tail flush (in case the input didn't end on an empty line).
        if let Some(event) = state.flush() {
            out.push(event);
        }
        out
    }

    #[test]
    fn decodes_simple_message_start() {
        let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let events = decode_all(sse);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"type\":\"message_start\"}");
        assert_eq!(events[0].raw.len(), 2);
    }

    #[test]
    fn joins_multiline_data_with_newline() {
        let sse = "event: content_block_delta\ndata: line1\ndata: line2\n\n";
        let events = decode_all(sse);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn comment_lines_are_dropped() {
        let sse = ": keepalive\nevent: ping\ndata: {}\n\nevent: message_start\ndata: {}\n\n";
        let events = decode_all(sse);
        // ping + message_start both flush.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[1].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn strips_single_leading_space_after_colon() {
        let sse = "event: message_delta\ndata: {\"x\":1}\n\n";
        let events = decode_all(sse);
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[test]
    fn cr_and_crlf_both_complete_a_line_immediately() {
        for text in ["data: value\r", "data: value\r\n"] {
            let (line, rest) = consume_line(text).unwrap();
            assert_eq!(line, "data: value");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn eof_consumes_a_standalone_cr_and_flushes_the_last_event() {
        let mut state = SseDecoderState::new();
        let mut buffer = "event: message_start\rdata: {}\r".to_string();
        let mut events = Vec::new();
        while let Some((line, rest)) = consume_line(&buffer) {
            let line = line.to_string();
            buffer = rest.to_string();
            if let Some(event) = state.decode_line(&line) {
                events.push(event);
            }
        }
        assert!(buffer.is_empty());
        if let Some(event) = state.flush() {
            events.push(event);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn split_utf8_code_point_is_reassembled_across_chunks() {
        let mut buffer = String::new();
        let mut pending = Vec::new();
        append_utf8_chunk(&mut buffer, &mut pending, b"data: \xe4");
        assert_eq!(buffer, "data: ");
        assert_eq!(pending, vec![0xe4]);

        append_utf8_chunk(&mut buffer, &mut pending, b"\xb8\xad\n\n");
        assert_eq!(buffer, "data: \u{4e2d}\n\n");
        assert!(pending.is_empty());
    }

    #[test]
    fn malformed_utf8_does_not_block_following_sse_bytes() {
        let mut buffer = String::new();
        let mut pending = Vec::new();
        append_utf8_chunk(&mut buffer, &mut pending, b"data: \xff");
        append_utf8_chunk(&mut buffer, &mut pending, b"ok\n\n");

        assert_eq!(buffer, "data: \u{fffd}ok\n\n");
        assert!(pending.is_empty());
    }

    #[test]
    fn nameless_event_has_none_event_field() {
        let sse = "data: only-data\n\n";
        let events = decode_all(sse);
        assert_eq!(events.len(), 1);
        assert!(events[0].event.is_none());
    }

    #[test]
    fn error_event_surfaces_provider_error() {
        let frame = ServerSentEvent {
            event: Some("error".to_string()),
            data: "rate limited".to_string(),
            raw: vec!["data: rate limited".to_string()],
        };
        let err = parse_anthropic_event(&frame).unwrap_err();
        assert!(matches!(err, AiError::Provider { code, .. } if code == "sse_error"));
    }

    #[test]
    fn non_message_event_is_skipped() {
        let frame = ServerSentEvent {
            event: Some("ping".to_string()),
            data: "{}".to_string(),
            raw: vec!["data: {}".to_string()],
        };
        let event = parse_anthropic_event(&frame).unwrap();
        assert!(matches!(event, AnthropicEvent::Skipped));
    }

    #[test]
    fn malformed_message_data_is_sse_error() {
        let frame = ServerSentEvent {
            event: Some("message_start".to_string()),
            data: "not json".to_string(),
            raw: vec!["data: not json".to_string()],
        };
        let err = parse_anthropic_event(&frame).unwrap_err();
        assert!(matches!(err, AiError::Sse { .. }));
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_body_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let mut stream = SseEventStream::new(response, signal);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next_event())
            .await
            .expect("cancellation must wake a pending response body read");
        assert!(matches!(result, Err(AiError::Abort { .. })));
        server.abort();
    }

    #[tokio::test]
    async fn body_decode_error_keeps_underlying_transport_detail() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\nConnection: close\r\n\r\nZZ\r\n",
                )
                .await
                .unwrap();
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let mut stream = SseEventStream::new(response, CancellationToken::new());
        let error = stream.next_event().await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("error reading sse body"));
        assert!(message.contains("error decoding response body"));
        assert!(message.contains("missing size digit"), "{message}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn body_timeout_is_reported_with_timeout_detail() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )
                .await
                .unwrap();
            // Keep the socket open without another timer racing the body deadline.
            std::future::pending::<()>().await;
            drop(socket);
        });

        let response = reqwest::Client::builder()
            // Allow the real loopback connection/headers to arrive on busy CI.
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        // Pause only after headers: this exercises the actual reqwest body
        // timeout without imposing a 30ms wall-clock budget on connection setup.
        tokio::time::pause();
        let mut stream =
            SseEventStream::new(response, CancellationToken::new()).with_idle_timeout(None);
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next_event())
            .await
            .unwrap()
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("error decoding response body"),
            "{message}"
        );
        assert!(message.to_lowercase().contains("timed out"), "{message}");
        server.abort();
    }
}
