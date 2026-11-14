//! Mirrors `packages/ai/src/providers/faux.ts` — the deterministic, scripted,
//! no-network provider used by every crate's tests. Hoisted into `pi-ai` (TS
//! keeps a copy in `packages/agent/src/faux.ts`; we centralize it here per the
//! plan so `pi-agent`/`pi-tools`/`pi-harness` tests reuse it without a circular
//! dependency on a higher crate).
//!
//! v1 scope (per plan): text/thinking/tool-call blocks, `streamWithDeltas`
//! throttling, abort via `CancellationToken`, usage estimate, and error-on-empty.
//! Deferred responses (long-poll) are out of scope for v1 — left as a TODO.

use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStream,
};
use crate::model::Model;
use crate::provider::{DeferredProvider, Provider, SimpleStreamOptions};
use crate::types::{
    Api, AssistantMessage, AssistantMessageEvent, Content, Context, DeferredHandle, DoneReason,
    ErrorReason, InputModality, StopReason, TextContent, ThinkingContent, ToolCall,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";
const DEFAULT_MODEL_NAME: &str = "Faux Model";
const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

// ----------------------------------------------------------------------------
// Builders — mirror fauxText / fauxThinking / fauxToolCall / fauxAssistantMessage
// ----------------------------------------------------------------------------

/// A scripted content block (text / thinking / tool-call). Mirrors TS
/// `FauxContentBlock = TextContent | ThinkingContent | ToolCall`.
#[derive(Debug, Clone)]
pub enum FauxBlock {
    Text(String),
    Thinking(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        id: String,
    },
}

impl FauxBlock {
    pub fn text<S: Into<String>>(s: S) -> Self {
        FauxBlock::Text(s.into())
    }
    pub fn thinking<S: Into<String>>(s: S) -> Self {
        FauxBlock::Thinking(s.into())
    }
    pub fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        FauxBlock::ToolCall {
            name: name.into(),
            arguments,
            id: faux_id("tool"),
        }
    }
}

/// Construct a `Content::Text` — mirrors TS `fauxText`.
pub fn faux_text<S: Into<String>>(s: S) -> Content {
    Content::Text(TextContent {
        kind: crate::types::TextContentType,
        text: s.into(),
        text_signature: None,
    })
}

/// Construct a `Content::Thinking` — mirrors TS `fauxThinking`.
pub fn faux_thinking<S: Into<String>>(s: S) -> Content {
    Content::Thinking(ThinkingContent {
        kind: crate::types::ThinkingContentType,
        thinking: s.into(),
        thinking_signature: None,
        redacted: false,
    })
}

/// Construct a `Content::ToolCall` — mirrors TS `fauxToolCall`.
pub fn faux_tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Content {
    Content::tool_call(faux_id("tool"), name, arguments)
}

fn faux_id(prefix: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{n}")
}

fn faux_now() -> i64 {
    static T: AtomicI64 = AtomicI64::new(0);
    T.fetch_add(1, Ordering::Relaxed)
}

/// Accepts a string, a single `FauxBlock`, or a `Vec<FauxBlock>` — mirrors TS
/// `string | FauxContentBlock | FauxContentBlock[]`.
#[derive(Debug, Clone)]
pub enum FauxContent {
    Text(String),
    One(FauxBlock),
    Many(Vec<FauxBlock>),
}

impl FauxContent {
    fn into_blocks(self) -> Vec<FauxBlock> {
        match self {
            FauxContent::Text(s) => vec![FauxBlock::Text(s)],
            FauxContent::One(b) => vec![b],
            FauxContent::Many(v) => v,
        }
    }
}

impl From<&str> for FauxContent {
    fn from(s: &str) -> Self {
        FauxContent::Text(s.to_string())
    }
}
impl From<String> for FauxContent {
    fn from(s: String) -> Self {
        FauxContent::Text(s)
    }
}
impl From<FauxBlock> for FauxContent {
    fn from(b: FauxBlock) -> Self {
        FauxContent::One(b)
    }
}
impl From<Vec<FauxBlock>> for FauxContent {
    fn from(v: Vec<FauxBlock>) -> Self {
        FauxContent::Many(v)
    }
}

/// Build an `AssistantMessage` from content + a stop reason. Mirrors TS
/// `fauxAssistantMessage`.
pub fn faux_assistant_message(
    content: impl Into<FauxContent>,
    stop_reason: StopReason,
) -> AssistantMessage {
    let blocks = content.into().into_blocks();
    let content: Vec<Content> = blocks
        .into_iter()
        .map(|b| match b {
            FauxBlock::Text(s) => faux_text(s),
            FauxBlock::Thinking(s) => faux_thinking(s),
            FauxBlock::ToolCall {
                name,
                arguments,
                id,
            } => Content::tool_call(id, name, arguments),
        })
        .collect();
    AssistantMessage {
        role: crate::types::AssistantRole,
        content,
        api: Api::Faux,
        provider: DEFAULT_PROVIDER.to_string(),
        model: DEFAULT_MODEL_ID.to_string(),
        response_model: None,
        response_id: None,
        usage: crate::types::Usage::zero(),
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: faux_now(),
    }
}

fn content_to_assistant_text(content: &[Content]) -> String {
    content
        .iter()
        .map(|c| match c {
            Content::Text(t) => t.text.clone(),
            Content::Thinking(t) => t.thinking.clone(),
            Content::ToolCall(tc) => format!("{}:{}", tc.name, tc.arguments),
            Content::Image(_) => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ----------------------------------------------------------------------------
// FauxStep / FauxScript — the scripted-response queue
// ----------------------------------------------------------------------------

/// A single scripted step. Mirrors the two arms of TS `FauxResponseStep`.
#[derive(Clone)]
pub enum FauxStep {
    /// A ready assistant message.
    Message(AssistantMessage),
    /// A dynamic factory evaluated at call time against a context snapshot.
    Factory(Arc<dyn Fn(&Context) -> AssistantMessage + Send + Sync>),
}

impl std::fmt::Debug for FauxStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FauxStep::Message(m) => f.debug_tuple("Message").field(m).finish(),
            FauxStep::Factory(_) => f.debug_struct("Factory").finish_non_exhaustive(),
        }
    }
}

impl FauxStep {
    pub fn message(m: AssistantMessage) -> Self {
        FauxStep::Message(m)
    }
    pub fn text<S: Into<String>>(s: S) -> Self {
        FauxStep::Message(faux_assistant_message(s.into(), StopReason::Stop))
    }
    pub fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        let blocks = vec![FauxBlock::tool_call(name, arguments)];
        FauxStep::Message(faux_assistant_message(blocks, StopReason::ToolUse))
    }
    pub fn factory<F>(f: F) -> Self
    where
        F: Fn(&Context) -> AssistantMessage + Send + Sync + 'static,
    {
        FauxStep::Factory(Arc::new(f))
    }
}

/// Builder for the faux provider's scripted response queue + streaming knobs.
/// Mirrors the slice of TS `RegisterFauxProviderOptions` v1 uses.
#[derive(Clone)]
pub struct FauxScript {
    steps: Arc<Mutex<Vec<FauxStep>>>,
    tokens_per_second: Option<f64>,
    min_token_size: usize,
    max_token_size: usize,
    abort_after: Option<std::time::Duration>,
}

impl Default for FauxScript {
    fn default() -> Self {
        Self {
            steps: Arc::new(Mutex::new(Vec::new())),
            tokens_per_second: None,
            min_token_size: DEFAULT_MIN_TOKEN_SIZE,
            max_token_size: DEFAULT_MAX_TOKEN_SIZE,
            abort_after: None,
        }
    }
}

impl std::fmt::Debug for FauxScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FauxScript")
            .field("pending", &self.steps.lock().map(|s| s.len()).unwrap_or(0))
            .field("tokens_per_second", &self.tokens_per_second)
            .field("abort_after", &self.abort_after)
            .finish()
    }
}

impl FauxScript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the scripted queue. Mirrors `setResponses`.
    pub fn set_responses(&self, steps: Vec<FauxStep>) {
        *self.steps.lock().unwrap() = steps;
    }

    /// Append to the queue. Mirrors `appendResponses`.
    pub fn append_responses(&self, steps: Vec<FauxStep>) {
        self.steps.lock().unwrap().extend(steps);
    }

    pub fn with_text<S: Into<String>>(mut self, s: S) -> Self {
        self.push(FauxStep::text(s));
        self
    }
    pub fn with_thinking<S: Into<String>>(mut self, s: S) -> Self {
        let blocks = vec![FauxBlock::thinking(s)];
        self.push(FauxStep::Message(faux_assistant_message(
            blocks,
            StopReason::Stop,
        )));
        self
    }
    pub fn with_tool_call(mut self, name: impl Into<String>, arguments: serde_json::Value) -> Self {
        self.push(FauxStep::tool_call(name, arguments));
        self
    }
    pub fn with_usage_estimate(self) -> Self {
        // Always on; the flag is API parity with the TS test helper.
        self
    }
    pub fn with_abort_after(mut self, d: std::time::Duration) -> Self {
        self.abort_after = Some(d);
        self
    }
    pub fn with_tokens_per_second(mut self, tps: f64) -> Self {
        self.tokens_per_second = Some(tps);
        self
    }

    fn push(&mut self, step: FauxStep) {
        self.steps.lock().unwrap().push(step);
    }

    fn shift(&self) -> Option<FauxStep> {
        let mut guard = self.steps.lock().unwrap();
        if guard.is_empty() {
            None
        } else {
            Some(guard.remove(0))
        }
    }

    pub fn pending_count(&self) -> usize {
        self.steps.lock().map(|s| s.len()).unwrap_or(0)
    }
}

// ----------------------------------------------------------------------------
// FauxProvider
// ----------------------------------------------------------------------------

/// Observable per-provider state — the v1 subset of TS `FauxProviderState`.
#[derive(Debug, Default)]
pub struct FauxProviderState {
    pub call_count: AtomicUsize,
    /// The `session_id` of each request, in call order.
    pub session_ids: Mutex<Vec<Option<String>>>,
}

/// The faux provider. Mirrors `createFauxCore`'s streaming surface; v1 exposes
/// the scripted queue + `tokens_per_second` + abort knobs.
pub struct FauxProvider {
    id: String,
    models: Vec<Model>,
    state: Arc<FauxProviderState>,
    script: FauxScript,
    /// Scripted answers to `poll_deferred`, consumed in order. Empty means the
    /// poll is unsupported, which is what a provider without a long-poll API
    /// should behave like.
    deferred_polls: Arc<Mutex<Vec<DeferredPoll>>>,
}

/// One scripted deferred poll result.
#[derive(Clone)]
pub enum DeferredPoll {
    /// The generation is ready.
    Message(AssistantMessage),
    /// Still not ready: another handle to poll.
    StillDeferred(DeferredHandle),
    /// The poll failed.
    Error(String),
}

impl FauxProvider {
    /// Queue one scripted deferred-poll result.
    pub fn push_deferred_poll(&self, poll: DeferredPoll) {
        self.deferred_polls.lock().unwrap().push(poll);
    }

    /// Build a faux provider carrying a single default model, sharing `script`.
    pub fn new(script: FauxScript) -> Arc<Self> {
        Self::with_models(script, vec![default_faux_model()])
    }

    /// Build with a custom model set.
    pub fn with_models(script: FauxScript, models: Vec<Model>) -> Arc<Self> {
        Arc::new(Self {
            id: DEFAULT_PROVIDER.to_string(),
            models,
            state: Arc::new(FauxProviderState::default()),
            script,
            deferred_polls: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn state(&self) -> &FauxProviderState {
        &self.state
    }
    pub fn script(&self) -> &FauxScript {
        &self.script
    }
    pub fn default_model(&self) -> &Model {
        &self.models[0]
    }
}

#[async_trait]
impl DeferredProvider for FauxProvider {
    /// Answer a poll from the scripted queue. An empty queue yields an `Error`
    /// event rather than a panic: a provider without deferred support must look
    /// like a failed poll, which is exactly what the caller has to handle.
    async fn stream_deferred(
        &self,
        _model: &Model,
        handle: &DeferredHandle,
        _opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let poll = {
            let mut queue = self.deferred_polls.lock().unwrap();
            if queue.is_empty() {
                None
            } else {
                Some(queue.remove(0))
            }
        };
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&_opts.signal);
        let placeholder = faux_assistant_message("", StopReason::Stop);
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(placeholder.clone()),
            })
            .await;
        let message = match poll {
            Some(DeferredPoll::Message(mut message)) => {
                message.deferred = None;
                message
            }
            Some(DeferredPoll::StillDeferred(next)) => {
                let mut message = placeholder;
                message.deferred = Some(next);
                message.stop_reason = StopReason::Deferred;
                message
            }
            Some(DeferredPoll::Error(error)) => faux_assistant_message(error, StopReason::Error),
            None => faux_assistant_message(
                format!("no scripted deferred poll for handle {}", handle.id),
                StopReason::Error,
            ),
        };
        // Terminal event, mirroring `stream_with_deltas`: failures are reported as
        // `Error` events so the caller handles both kinds of provider call alike.
        match message.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                let reason = if matches!(message.stop_reason, StopReason::Aborted) {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                producer
                    .push(AssistantMessageEvent::Error {
                        reason,
                        error: message.clone(),
                    })
                    .await;
            }
            other => {
                let reason = match other {
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                producer
                    .push(AssistantMessageEvent::Done {
                        reason,
                        message: message.clone(),
                    })
                    .await;
            }
        }
        producer.close();
        stream
    }
}

#[async_trait]
impl Provider for FauxProvider {
    /// Advertise the deferred capability, so `resume_deferred` can find it.
    fn deferred(&self) -> Option<&dyn DeferredProvider> {
        Some(self)
    }

    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> crate::event_stream::AssistantMessageEventStream {
        let (mut prod, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);

        // Capture everything the producer task needs by value.
        let state = Arc::clone(&self.state);
        let script = self.script.clone();
        let id = self.id.clone();
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        let mut opts = opts.clone();
        opts.signal = prod.cancellation_token();

        tokio::spawn(async move {
            state.call_count.fetch_add(1, Ordering::Relaxed);
            state
                .session_ids
                .lock()
                .unwrap()
                .push(opts.session_id.clone());

            let step = script.shift();
            let mut message = match step {
                None => AssistantMessage::terminal(
                    model.api.clone(),
                    id.clone(),
                    model.id.clone(),
                    StopReason::Error,
                    "No more faux responses queued",
                    faux_now(),
                ),
                Some(s) => {
                    let m = {
                        // resolve_step borrows `self`, but `self` isn't owned
                        // here — inline the resolution to avoid the borrow.
                        let mut m = match s {
                            FauxStep::Message(m) => m,
                            FauxStep::Factory(f) => f(&ctx),
                        };
                        m.api = model.api.clone();
                        m.provider = id.clone();
                        m.model = model.id.clone();
                        m
                    };
                    m
                }
            };
            message = with_usage_estimate(message, &ctx, &opts);

            // Cancellation: honor the caller's signal, and (optionally) an
            // internal abort-after timer. A detached timer task cancels the
            // child token after `abort_after`; firing after completion is a
            // harmless no-op (the token is no longer polled).
            let child = opts.signal.clone();
            let timer_token = child.clone();
            let _timer = script.abort_after.map(|d| {
                tokio::spawn(async move {
                    tokio::time::sleep(d).await;
                    timer_token.cancel();
                })
            });

            stream_with_deltas(
                &mut prod,
                &mut message,
                script.min_token_size,
                script.max_token_size,
                script.tokens_per_second,
                &child,
            )
            .await;
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

// ----------------------------------------------------------------------------

fn default_faux_model() -> Model {
    let mut m = Model::new(
        DEFAULT_MODEL_ID,
        DEFAULT_MODEL_NAME,
        Api::Faux,
        DEFAULT_PROVIDER,
        DEFAULT_BASE_URL,
    );
    m.input = vec![InputModality::Text, InputModality::Image];
    m.context_window = 128_000;
    m.max_tokens = 16_384;
    m
}

// ----------------------------------------------------------------------------
// Usage estimate — mirrors withUsageEstimate (cold-cache arm)
// ----------------------------------------------------------------------------

fn estimate_tokens(text: &str) -> i64 {
    ((text.len() as f64) / 4.0).ceil() as i64
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    ctx: &Context,
    _opts: &SimpleStreamOptions,
) -> AssistantMessage {
    let prompt_text = serialize_context(ctx);
    let prompt_tokens = estimate_tokens(&prompt_text);
    let output_tokens = estimate_tokens(&content_to_assistant_text(&message.content));
    let input = prompt_tokens;
    message.usage = crate::types::Usage {
        input,
        output: output_tokens,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output_tokens,
        cost: crate::types::UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    };
    message
}

fn serialize_context(ctx: &Context) -> String {
    let mut parts = Vec::new();
    if let Some(sys) = ctx.system_prompt.as_deref() {
        parts.push(format!("system:{sys}"));
    }
    for msg in &ctx.messages {
        match msg {
            crate::types::Message::User(u) => {
                parts.push(format!("user:{}", user_content_text(&u.content)));
            }
            crate::types::Message::Assistant(a) => {
                parts.push(format!(
                    "assistant:{}",
                    content_to_assistant_text(&a.content)
                ));
            }
            crate::types::Message::ToolResult(t) => {
                let text: String = t
                    .content
                    .iter()
                    .map(|c| match c {
                        Content::Text(t) => t.text.clone(),
                        _ => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(format!("toolResult:{}:{}", t.tool_name, text));
            }
        }
    }
    parts.join("\n\n")
}

fn user_content_text(c: &crate::types::UserContent) -> String {
    match c {
        crate::types::UserContent::Text(s) => s.clone(),
        crate::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                Content::Text(t) => t.text.clone(),
                Content::Image(i) => format!("[image:{}:{}]", i.mime_type, i.data.len()),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// ----------------------------------------------------------------------------
// streamWithDeltas — faithful port
// ----------------------------------------------------------------------------

/// Stream `message`'s content blocks as start/delta*/end events and a terminal
/// `Done`/`Error`. Mirrors TS `streamWithDeltas`. Honours `signal` between every
/// chunk; cancellation emits `Error { reason: Aborted }` and ends the stream.
async fn stream_with_deltas(
    prod: &mut crate::event_stream::AssistantMessageEventStreamProducer,
    message: &mut AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    signal: &CancellationToken,
) {
    let mut partial = AssistantMessage {
        content: Vec::new(),
        stop_reason: StopReason::Pending,
        ..message.clone()
    };

    if signal.is_cancelled() {
        let aborted = create_aborted_message(&partial);
        prod.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: aborted.clone(),
        })
        .await;
        *message = aborted;
        return;
    }

    prod.push(AssistantMessageEvent::Start {
        partial: Arc::new(partial.clone()),
    })
    .await;

    let mut abort_into_message = None::<AssistantMessage>;
    for index in 0..message.content.len() {
        if signal.is_cancelled() {
            abort_into_message = Some(create_aborted_message(&partial));
            break;
        }

        let block = message.content[index].clone();
        match &block {
            Content::Thinking(tc) => {
                partial.content.push(faux_thinking(""));
                prod.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: Arc::new(partial.clone()),
                })
                .await;
                for chunk in
                    split_string_by_token_size(&tc.thinking, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_cancelled() {
                        abort_into_message = Some(create_aborted_message(&partial));
                        break;
                    }
                    if let Content::Thinking(slot) = &mut partial.content[index] {
                        slot.thinking.push_str(&chunk);
                    }
                    prod.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: Arc::new(partial.clone()),
                    })
                    .await;
                }
                if abort_into_message.is_some() {
                    break;
                }
                prod.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: tc.thinking.clone(),
                    partial: Arc::new(partial.clone()),
                })
                .await;
            }
            Content::Text(tc) => {
                partial.content.push(faux_text(""));
                prod.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: Arc::new(partial.clone()),
                })
                .await;
                for chunk in split_string_by_token_size(&tc.text, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_cancelled() {
                        abort_into_message = Some(create_aborted_message(&partial));
                        break;
                    }
                    if let Content::Text(slot) = &mut partial.content[index] {
                        slot.text.push_str(&chunk);
                    }
                    prod.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: Arc::new(partial.clone()),
                    })
                    .await;
                }
                if abort_into_message.is_some() {
                    break;
                }
                prod.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: tc.text.clone(),
                    partial: Arc::new(partial.clone()),
                })
                .await;
            }
            Content::ToolCall(tc) => {
                partial.content.push(Content::tool_call(
                    tc.id.clone(),
                    tc.name.clone(),
                    serde_json::Value::Object(Default::default()),
                ));
                prod.push(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: Arc::new(partial.clone()),
                })
                .await;
                for chunk in split_string_by_token_size(
                    &tc.arguments.to_string(),
                    min_token_size,
                    max_token_size,
                ) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_cancelled() {
                        abort_into_message = Some(create_aborted_message(&partial));
                        break;
                    }
                    prod.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: Arc::new(partial.clone()),
                    })
                    .await;
                }
                if abort_into_message.is_some() {
                    break;
                }
                if let Content::ToolCall(slot) = &mut partial.content[index] {
                    slot.arguments = tc.arguments.clone();
                }
                prod.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: ToolCall {
                        kind: tc.kind,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        thought_signature: tc.thought_signature.clone(),
                        namespace: tc.namespace.clone(),
                        recorded: tc.recorded.clone(),
                    },
                    partial: Arc::new(partial.clone()),
                })
                .await;
            }
            Content::Image(_) => {
                // Faux doesn't stream image blocks — mirrors TS (no image arm).
            }
        }
    }

    if let Some(aborted) = abort_into_message {
        prod.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: aborted.clone(),
        })
        .await;
        *message = aborted;
        return;
    }

    match message.stop_reason {
        StopReason::Pending => {
            let err = AssistantMessage::terminal(
                message.api.clone(),
                message.provider.clone(),
                message.model.clone(),
                StopReason::Error,
                "Faux response ended without a stop reason",
                faux_now(),
            );
            prod.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: err.clone(),
            })
            .await;
            *message = err;
        }
        StopReason::Error | StopReason::Aborted => {
            let reason = matches!(message.stop_reason, StopReason::Aborted)
                .then_some(ErrorReason::Aborted)
                .unwrap_or(ErrorReason::Error);
            prod.push(AssistantMessageEvent::Error {
                reason,
                error: message.clone(),
            })
            .await;
        }
        StopReason::Stop | StopReason::Length | StopReason::ToolUse | StopReason::Deferred => {
            let reason = match message.stop_reason {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Deferred => DoneReason::Deferred,
                _ => unreachable!(),
            };
            prod.push(AssistantMessageEvent::Done {
                reason,
                message: message.clone(),
            })
            .await;
        }
    }
}

fn create_aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    let mut m = partial.clone();
    m.stop_reason = StopReason::Aborted;
    m.error_message = Some("Request was aborted".to_string());
    m.timestamp = faux_now();
    m
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second {
        Some(tps) if tps > 0.0 => {
            let delay_ms = (estimate_tokens(chunk) as f64 / tps) * 1000.0;
            if delay_ms > 0.0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        _ => tokio::task::yield_now().await,
    }
}

/// Split `text` into chunks of `[min, max]` "tokens" (×4 chars), walking UTF-8
/// char boundaries so multi-byte text is safe. Mirrors TS
/// `splitStringByTokenSize`.
fn split_string_by_token_size(text: &str, min: usize, max: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let span = max.saturating_sub(min).max(1);
    let chars: Vec<&str> = text.char_indices().map(|(i, _)| &text[i..]).collect();
    let n = chars.len();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < n {
        let token_size = min + (faux_next_u32() as usize % (span + 1));
        let char_size = (token_size * 4).max(1);
        let end = (i + char_size).min(n);
        // Reconstruct the substring spanning chars[i..end] by slicing from the
        // start offset of chars[i] to the start offset of chars[end] (or len).
        let start_byte = text.len() - chars[i].len();
        let end_byte = if end < n {
            text.len() - chars[end].len()
        } else {
            text.len()
        };
        chunks.push(text[start_byte..end_byte].to_string());
        i = end;
    }
    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

fn faux_next_u32() -> u32 {
    static SEED: AtomicU32 = AtomicU32::new(0x9E3779B9);
    // xorshift32 — deterministic, no RNG crate. Tests don't assert on exact
    // chunk boundaries, only on event ordering + final content.
    let mut x = SEED.fetch_add(1, Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}
