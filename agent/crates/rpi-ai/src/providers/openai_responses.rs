//! OpenAI Responses API provider (`/v1/responses`).

use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::{Model, StreamingProtocolCompat};
use crate::provider::{CacheRetention, Provider, SimpleStreamOptions};
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::anthropic::json_parse::parse_streaming_json;
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;
use crate::providers::{DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
use crate::strict_schema::strict_tool_parameters;
use crate::types::{
    Api, AssistantMessage, AssistantMessageEvent, ConstrainedSamplingConfig, Content, Context,
    DoneReason, ErrorReason, ImageContent, InputModality, Message, StopReason, ThinkingLevel, Tool,
    ToolCall, ToolCallType, Usage, UserContent,
};
use crate::AiError;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct OpenAiResponsesProvider {
    id: String,
    api_key: Option<String>,
    allow_env_api_key: bool,
    require_auth: bool,
    http: reqwest::Client,
    models: Vec<Model>,
}
pub fn openai_responses_models() -> Vec<Model> {
    let mut model = Model::new(
        "gpt-6-astra",
        "GPT-6 Astra",
        Api::OpenaiResponses,
        "openai",
        "https://api.openai.com/v1",
    );
    model.reasoning = true;
    model.context_window = 1_000_000;
    model.max_tokens = 128_000;
    model.compat = Some(StreamingProtocolCompat::OpenaiResponses(
        crate::model::OpenaiResponsesCompat {
            supports_developer_role: Some(true),
            supports_long_cache_retention: Some(true),
            supports_strict_mode: Some(true),
            supports_openai_grammar_tools: Some(true),
            ..Default::default()
        },
    ));
    vec![model]
}
impl OpenAiResponsesProvider {
    pub fn with_models(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let id = id.into();
        let allow_env_api_key = id == "openai";
        Self {
            id,
            api_key,
            allow_env_api_key,
            require_auth: true,
            http,
            models,
        }
    }

    /// Build a compatible custom provider without inheriting OpenAI's
    /// process-wide credential.
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            allow_env_api_key: false,
            require_auth: true,
            http,
            models,
        }
    }

    /// Explicit anonymous authentication for local/proxy Responses endpoints.
    pub fn with_models_without_auth(
        id: impl Into<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let mut provider = Self::with_models_without_env_api_key(id, None, http, models);
        provider.require_auth = false;
        provider
    }

    /// Reuse Responses serialization and SSE handling for a provider-native
    /// endpoint whose URL and authentication have already been resolved.
    pub(crate) fn stream_simple_with_endpoint(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
        url: String,
        mut headers: std::collections::BTreeMap<String, String>,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let deployment_key = headers
            .keys()
            .find(|name| name.eq_ignore_ascii_case("x-omp-azure-deployment"))
            .cloned();
        let deployment = deployment_key.and_then(|name| headers.remove(&name));
        let mut body = build_request(model, ctx, opts);
        if let Some(deployment) = deployment {
            body["model"] = json!(deployment);
        }
        let preparation = Ok(PreparedRequest {
            headers,
            body,
            custom_tool_properties: grammar_tool_properties(model, &ctx.tools),
            url: Some(url),
        });
        let model = model.clone();
        let http = self.http.clone();
        let mut opts = opts.clone();
        opts.signal = producer.cancellation_token();
        tokio::spawn(async move {
            run_stream(&mut producer, http, &model, preparation, &opts).await;
        });
        stream
    }
}
struct PreparedRequest {
    headers: std::collections::BTreeMap<String, String>,
    body: Value,
    custom_tool_properties: HashMap<String, String>,
    url: Option<String>,
}

fn prepare_request<F>(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    provider_key: &Option<String>,
    allow_env_api_key: bool,
    require_auth: bool,
    build_body: F,
) -> Result<PreparedRequest, String>
where
    F: FnOnce(&Model, &Context, &SimpleStreamOptions) -> Value,
{
    let mut headers = std::collections::BTreeMap::new();
    for extra in [model.headers.as_ref(), opts.headers.as_ref()]
        .into_iter()
        .flatten()
    {
        for (name, value) in extra {
            crate::provider::set_http_header(&mut headers, name, Some(value));
        }
    }
    let key = resolve_api_key(provider_key, opts, allow_env_api_key);
    apply_api_key(&mut headers, key);
    if require_auth
        && !headers.keys().any(|name| {
            name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("x-api-key")
                || name.eq_ignore_ascii_case("cf-aig-authorization")
        })
    {
        return Err("No API key".to_string());
    }

    // Keep the original auth/header ordering: only after it succeeds do we
    // convert history and retain the minimal tool metadata needed by SSE.
    Ok(PreparedRequest {
        headers,
        body: build_body(model, ctx, opts),
        custom_tool_properties: grammar_tool_properties(model, &ctx.tools),
        url: None,
    })
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
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
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let http = self.http.clone();
        let key = self.api_key.clone();
        let allow_env_api_key = self.allow_env_api_key;
        // Prepare headers while the caller's Context is borrowed. The helper
        // returns an error result for the producer task, so auth failure still
        // follows the original terminal Error event path without a dummy body.
        let preparation = prepare_request(
            model,
            ctx,
            opts,
            &key,
            allow_env_api_key,
            self.require_auth,
            build_request,
        );
        let model = model.clone();
        let mut opts = opts.clone();
        opts.signal = producer.cancellation_token();
        tokio::spawn(async move {
            run_stream(&mut producer, http, &model, preparation, &opts).await;
        });
        stream
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseSlotKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Debug, Clone)]
struct ResponseSlot {
    kind: ResponseSlotKind,
    content_index: usize,
    item_id: Option<String>,
    call_id: Option<String>,
    name: String,
    arguments: String,
    /// Regular function deltas must include output_item.added's raw prefix once.
    function_arguments_started: bool,
    /// Custom Responses tools stream a raw string in `input` rather than a
    /// JSON object in `arguments`. Keep that wire distinction while exposing
    /// the provider-neutral object shape to the rest of pi.
    custom: bool,
    custom_input_property: Option<String>,
    custom_input_started: bool,
    custom_input_closed: bool,
    ended: bool,
}

#[derive(Debug)]
struct ResponsesStreamState {
    slots: HashMap<usize, ResponseSlot>,
    next_index: usize,
    /// Grammar-constrained tools are represented by Responses as
    /// `custom_tool_call` items. The map stores the sole required string
    /// property used to wrap their streamed input for the neutral ToolCall
    /// shape. A missing entry falls back to the protocol's `input` property.
    custom_tool_properties: HashMap<String, String>,
    /// Sequence numbers are intended to be unique. Suppress exact replays only
    /// after observing more than one distinct number; until then, identical
    /// payloads may be legitimate repeated deltas from a gateway that emits a
    /// constant sequence. A collision disables sequence-based deduplication for
    /// the rest of the stream so malformed metadata can never drop content.
    seen_sequences: HashMap<i64, Value>,
    sequence_numbers_reliable: bool,
    sequence_numbers_unreliable: bool,
    terminal: bool,
    /// Text/thinking deltas batched but not yet emitted, with the content index
    /// they belong to. Every event carries a full `partial: Arc<AssistantMessage>`
    /// snapshot, so one event per wire delta made a long stream quadratic in the
    /// message length — see [`DELTA_FLUSH_BYTES`] for the measured numbers.
    pending_text: String,
    pending_text_index: Option<usize>,
    pending_thinking: String,
    pending_thinking_index: Option<usize>,
    /// When a batched delta was last emitted, for [`DELTA_FLUSH_INTERVAL`].
    last_flush: Option<tokio::time::Instant>,
}

impl Default for ResponsesStreamState {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            next_index: 0,
            custom_tool_properties: HashMap::new(),
            seen_sequences: HashMap::new(),
            sequence_numbers_reliable: false,
            sequence_numbers_unreliable: false,
            terminal: false,
            pending_text: String::new(),
            pending_text_index: None,
            pending_thinking: String::new(),
            pending_thinking_index: None,
            last_flush: None,
        }
    }
}

impl ResponsesStreamState {
    fn with_tools(custom_tool_properties: HashMap<String, String>) -> Self {
        Self {
            custom_tool_properties,
            ..Self::default()
        }
    }

    /// Whether a pending batch should be emitted now: past the byte threshold,
    /// or older than the flush interval.
    fn should_flush(&self, pending: &str) -> bool {
        pending.len() >= DELTA_FLUSH_BYTES
            || self
                .last_flush
                .is_none_or(|last| last.elapsed() >= DELTA_FLUSH_INTERVAL)
    }

    fn flush_deadline(&self) -> Option<tokio::time::Instant> {
        if !self.pending_text.is_empty() || !self.pending_thinking.is_empty() {
            self.last_flush.map(|last| last + DELTA_FLUSH_INTERVAL)
        } else {
            None
        }
    }

    /// Emit whatever text/thinking is batched. Called at every item boundary
    /// (`finish_item` / `finish_open_slots`, i.e. before each `*End`) and
    /// whenever the stream switches channel, so no delta is dropped or
    /// reordered.
    async fn flush_pending(
        &mut self,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.flush_text(out, p).await;
        self.flush_thinking(out, p).await;
    }

    async fn flush_text(
        &mut self,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        if self.pending_text.is_empty() {
            return;
        }
        let Some(content_index) = self.pending_text_index else {
            self.pending_text.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_text);
        self.last_flush = Some(tokio::time::Instant::now());
        p.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            partial: Arc::new(out.clone()),
        })
        .await;
    }

    async fn flush_thinking(
        &mut self,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        if self.pending_thinking.is_empty() {
            return;
        }
        let Some(content_index) = self.pending_thinking_index else {
            self.pending_thinking.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_thinking);
        self.last_flush = Some(tokio::time::Instant::now());
        p.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            partial: Arc::new(out.clone()),
        })
        .await;
    }

    fn custom_property_for(&self, name: &str) -> String {
        self.custom_tool_properties
            .get(name)
            .cloned()
            .unwrap_or_else(|| "input".to_string())
    }

    fn mark_sequence(&mut self, value: &Value) -> bool {
        let Some(sequence) = value.get("sequence_number").and_then(Value::as_i64) else {
            return true;
        };
        if self.sequence_numbers_unreliable {
            return true;
        }
        if let Some(previous) = self.seen_sequences.get(&sequence) {
            if previous == value {
                return !self.sequence_numbers_reliable;
            }
            self.sequence_numbers_unreliable = true;
            self.seen_sequences.clear();
            return true;
        }
        self.sequence_numbers_reliable = !self.seen_sequences.is_empty();
        self.seen_sequences.insert(sequence, value.clone());
        true
    }

    fn reserve_index(&mut self) -> usize {
        while self.slots.contains_key(&self.next_index) {
            self.next_index = self.next_index.saturating_add(1);
        }
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
    }

    fn find_by_item_id(&self, item_id: &str, custom: bool) -> Option<usize> {
        self.slots.iter().find_map(|(index, slot)| {
            (slot.item_id.as_deref() == Some(item_id) && slot.custom == custom).then_some(*index)
        })
    }

    fn find_open_delta_slot(
        &self,
        value: &Value,
        kind: ResponseSlotKind,
        custom: bool,
    ) -> Option<usize> {
        let call_id = value
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let item_id = value
            .get("item_id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());

        // A few compatible gateways omit both output_index and item_id on
        // consecutive deltas. Reuse an open slot when its metadata identifies
        // the same call; otherwise prefer the most recently opened slot. The
        // latter is the only deterministic choice when the wire carries no
        // discriminator at all.
        let candidates = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.kind == kind && slot.custom == custom && !slot.ended);
        if item_id.is_some() || call_id.is_some() || name.is_some() {
            let matching = candidates
                .clone()
                .filter(|(_, slot)| {
                    if call_id.is_some() || name.is_some() {
                        call_id.map_or(true, |call_id| slot.call_id.as_deref() == Some(call_id))
                            && name.map_or(true, |name| slot.name == name)
                    } else {
                        item_id.map_or(true, |item_id| slot.item_id.as_deref() == Some(item_id))
                    }
                })
                .map(|(index, _)| *index)
                .max();
            return matching;
        }
        candidates.map(|(index, _)| *index).max()
    }

    fn find_for_item(&self, item: &Value, index_hint: Option<usize>) -> Option<usize> {
        let item_type = item.get("type").and_then(Value::as_str);
        let kind = response_slot_kind(item_type)?;
        let custom = item_type == Some("custom_tool_call");
        if let Some(index) = index_hint.filter(|index| {
            self.slots
                .get(index)
                .is_some_and(|slot| slot.kind == kind && slot.custom == custom)
        }) {
            return Some(index);
        }
        if let Some(item_id) = item.get("id").and_then(Value::as_str) {
            if let Some(index) = self.slots.iter().find_map(|(index, slot)| {
                (slot.kind == kind
                    && slot.custom == custom
                    && slot.item_id.as_deref() == Some(item_id))
                .then_some(*index)
            }) {
                return Some(index);
            }
        }
        let call_id = item.get("call_id").and_then(Value::as_str);
        let name = item.get("name").and_then(Value::as_str);
        self.slots.iter().find_map(|(index, slot)| {
            if slot.kind != kind || slot.custom != custom {
                return None;
            }
            if let Some(call_id) = call_id {
                if slot.call_id.as_deref() != Some(call_id) {
                    return None;
                }
            }
            if let Some(name) = name {
                if !name.is_empty() && slot.name != name {
                    return None;
                }
            }
            Some(*index)
        })
    }

    async fn add_item(
        &mut self,
        index: usize,
        item: &Value,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) -> Option<usize> {
        let item_type = item.get("type").and_then(Value::as_str);
        let kind = response_slot_kind(item_type)?;
        // A structural snapshot must never contain an un-emitted batch.
        self.flush_pending(out, p).await;
        let custom = item_type == Some("custom_tool_call");
        apply_message_phase(out, item);
        let item_name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let item_custom_property = item_name
            .filter(|_| custom)
            .map(|name| self.custom_property_for(name));
        if let Some(slot) = self.slots.get_mut(&index) {
            if slot.kind != kind || slot.custom != custom {
                return None;
            }
            update_slot_metadata(slot, item);
            if custom {
                if let Some(property) = item_custom_property {
                    slot.custom_input_property = Some(property);
                }
                if let Some(input) = item
                    .get("input")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    slot.arguments = input.to_string();
                    if let Some(Content::ToolCall(call)) = out.content.get_mut(slot.content_index) {
                        call.arguments = custom_tool_arguments(
                            slot.custom_input_property.as_deref().unwrap_or("input"),
                            input,
                        );
                    }
                }
            }
            return Some(slot.content_index);
        }

        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let arguments = item
            .get("arguments")
            .or_else(|| item.get("input"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let custom_input_property = custom.then(|| self.custom_property_for(&name));
        let content_index = out.content.len();
        match kind {
            ResponseSlotKind::Thinking => out.content.push(Content::thinking("")),
            ResponseSlotKind::Text => out.content.push(Content::text("")),
            ResponseSlotKind::ToolCall => out.content.push(Content::tool_call(
                compose_tool_call_id(call_id.as_deref(), item_id.as_deref()),
                name.clone(),
                if custom {
                    custom_tool_arguments(
                        custom_input_property.as_deref().unwrap_or("input"),
                        &arguments,
                    )
                } else {
                    parse_streaming_json(Some(&arguments))
                },
            )),
        }
        self.slots.insert(
            index,
            ResponseSlot {
                kind,
                content_index,
                item_id,
                call_id,
                name,
                arguments,
                function_arguments_started: false,
                custom,
                custom_input_property,
                custom_input_started: false,
                custom_input_closed: false,
                ended: false,
            },
        );
        match kind {
            ResponseSlotKind::Thinking => {
                p.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::new(out.clone()),
                })
                .await
            }
            ResponseSlotKind::Text => {
                p.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: Arc::new(out.clone()),
                })
                .await
            }
            ResponseSlotKind::ToolCall => {
                p.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::new(out.clone()),
                })
                .await
            }
        };
        Some(content_index)
    }

    async fn ensure_delta_slot(
        &mut self,
        value: &Value,
        kind: ResponseSlotKind,
        custom: bool,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) -> Option<usize> {
        let explicit = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let item_id = value
            .get("item_id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str);
        let event_name = value.get("name").and_then(Value::as_str);
        let event_custom_property = event_name
            .filter(|_| custom)
            .map(|name| self.custom_property_for(name));
        let index = explicit
            .filter(|index| {
                self.slots
                    .get(index)
                    .map_or(true, |slot| slot.kind == kind && slot.custom == custom)
            })
            .or_else(|| item_id.and_then(|id| self.find_by_item_id(id, custom)))
            .or_else(|| self.find_open_delta_slot(value, kind, custom))
            .unwrap_or_else(|| self.reserve_index());
        if !self.slots.contains_key(&index) {
            let mut item = json!({
                "type": response_slot_type(kind, custom),
                "id": item_id.unwrap_or_default(),
                "call_id": value.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                "name": value.get("name").and_then(Value::as_str).unwrap_or_default(),
            });
            item[if custom { "input" } else { "arguments" }] = Value::String(String::new());
            self.add_item(index, &item, out, p).await;
        } else {
            if let Some(slot) = self.slots.get_mut(&index) {
                if slot.kind != kind || slot.custom != custom || slot.ended {
                    return None;
                }
                if let Some(id) = item_id.filter(|id| !id.is_empty()) {
                    slot.item_id = Some(id.to_string());
                }
                if let Some(call_id) = value.get("call_id").and_then(Value::as_str) {
                    if !call_id.is_empty() {
                        slot.call_id = Some(call_id.to_string());
                    }
                }
                if let Some(name) = value.get("name").and_then(Value::as_str) {
                    if !name.is_empty() {
                        slot.name = name.to_string();
                        if slot.custom_input_property.is_none() {
                            slot.custom_input_property = event_custom_property.clone();
                        }
                    }
                }
            }
        }
        self.slots.contains_key(&index).then_some(index)
    }

    async fn append_text(
        &mut self,
        index: usize,
        delta: &str,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
        thinking: bool,
    ) {
        let Some(slot) = self.slots.get(&index).cloned() else {
            return;
        };
        if slot.ended {
            return;
        }
        if thinking {
            // Flush before modifying out: snapshots must not expose the next
            // channel's content before its delta has been emitted.
            self.flush_text(out, p).await;
            if let Some(Content::Thinking(block)) = out.content.get_mut(slot.content_index) {
                block.thinking.push_str(delta);
            }
            self.pending_thinking_index = Some(slot.content_index);
            self.pending_thinking.push_str(delta);
            if self.should_flush(&self.pending_thinking) {
                self.flush_thinking(out, p).await;
            }
        } else {
            self.flush_thinking(out, p).await;
            if let Some(Content::Text(block)) = out.content.get_mut(slot.content_index) {
                block.text.push_str(delta);
            }
            // Batch: `partial` is a full message clone per event.
            self.pending_text_index = Some(slot.content_index);
            self.pending_text.push_str(delta);
            if self.should_flush(&self.pending_text) {
                self.flush_text(out, p).await;
            }
        }
    }

    async fn append_tool_delta(
        &mut self,
        index: usize,
        delta: &str,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.flush_pending(out, p).await;
        let Some(slot) = self.slots.get_mut(&index) else {
            return;
        };
        if slot.kind != ResponseSlotKind::ToolCall || slot.ended {
            return;
        }
        slot.arguments.push_str(delta);
        // The initial raw arguments from output_item.added have not been
        // emitted as a delta yet. Preserve their exact bytes on the first
        // fragment, then emit only suffixes (without changing snapshots).
        let wire_delta = if slot.function_arguments_started {
            delta.to_string()
        } else {
            slot.function_arguments_started = true;
            slot.arguments.clone()
        };
        if let Some(Content::ToolCall(call)) = out.content.get_mut(slot.content_index) {
            call.arguments = parse_streaming_json(Some(&slot.arguments));
            if !slot.name.is_empty() {
                call.name = slot.name.clone();
            }
            call.id = compose_tool_call_id(slot.call_id.as_deref(), slot.item_id.as_deref());
        }
        p.push(AssistantMessageEvent::ToolCallDelta {
            content_index: slot.content_index,
            delta: wire_delta,
            partial: Arc::new(out.clone()),
        })
        .await;
    }

    /// Append a custom-tool input fragment while exposing the neutral tool
    /// call as a one-property JSON object. The Responses wire protocol carries
    /// the raw input string in `response.custom_tool_call_input.*` events; the
    /// surrounding JSON is synthesized only for the stream delta.
    async fn append_custom_tool_input(
        &mut self,
        index: usize,
        next_input: &str,
        close: bool,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.flush_pending(out, p).await;
        let Some(slot) = self.slots.get_mut(&index) else {
            return;
        };
        if slot.kind != ResponseSlotKind::ToolCall || !slot.custom {
            return;
        }
        if slot.custom_input_closed {
            // A repeated done event is harmless when it repeats the final
            // input. A changed value would violate the provider's monotonic
            // input contract; ignore it rather than emitting a corrupt delta.
            return;
        }

        // Before the first emitted fragment, `slot.arguments` may contain the
        // initial `input` from output_item.added. That value has not appeared in
        // the neutral stream yet, so the first delta must include it in full.
        let previous_emitted = if slot.custom_input_started {
            slot.arguments.clone()
        } else {
            String::new()
        };
        if !next_input.starts_with(&previous_emitted) {
            return;
        }
        let input_delta = &next_input[previous_emitted.len()..];
        if !close && input_delta.is_empty() {
            return;
        }
        let property = slot.custom_input_property.as_deref().unwrap_or("input");
        let mut wire_delta = String::new();
        if !slot.custom_input_started {
            wire_delta.push('{');
            wire_delta.push_str(
                &serde_json::to_string(property).unwrap_or_else(|_| "\"input\"".to_string()),
            );
            wire_delta.push_str(":\"");
            slot.custom_input_started = true;
        }
        if let Ok(escaped) = serde_json::to_string(input_delta) {
            wire_delta.push_str(&escaped[1..escaped.len().saturating_sub(1)]);
        } else {
            return;
        }
        slot.arguments = next_input.to_string();
        if close {
            wire_delta.push_str("\"}");
            slot.custom_input_closed = true;
        }

        if let Some(Content::ToolCall(call)) = out.content.get_mut(slot.content_index) {
            call.arguments = custom_tool_arguments(property, next_input);
            if !slot.name.is_empty() {
                call.name = slot.name.clone();
            }
            call.id = compose_tool_call_id(slot.call_id.as_deref(), slot.item_id.as_deref());
        }
        if !wire_delta.is_empty() {
            p.push(AssistantMessageEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: wire_delta,
                partial: Arc::new(out.clone()),
            })
            .await;
        }
    }

    async fn finish_item(
        &mut self,
        index: usize,
        item: &Value,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        // Item boundary: the batched tail must be emitted before the `*End`
        // event that closes this item, otherwise it would appear in the final
        // message but never in the delta stream.
        self.flush_pending(out, p).await;
        apply_message_phase(out, item);
        if let Some(slot) = self.slots.get_mut(&index) {
            update_slot_metadata(slot, item);
        }
        // Custom calls need a closing JSON delta even when the gateway sends
        // only output_item.done (without a separate custom-input.done event).
        if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
            let input = item
                .get("input")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| self.slots.get(&index).map(|slot| slot.arguments.clone()))
                .unwrap_or_default();
            self.append_custom_tool_input(index, &input, true, out, p)
                .await;
        }
        let Some(slot) = self.slots.get_mut(&index) else {
            return;
        };
        if slot.ended {
            // Azure and a few compatible gateways omit encrypted reasoning
            // content from `response.output_item.done` and only include it in
            // the terminal response output. Keep the richer terminal item so
            // a subsequent stateless request can replay the reasoning block.
            if slot.kind == ResponseSlotKind::Thinking {
                if let Some(Content::Thinking(block)) = out.content.get_mut(slot.content_index) {
                    let has_encrypted_content = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty());
                    let has_item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty());
                    if has_item_id && (has_encrypted_content || block.thinking_signature.is_none())
                    {
                        block.thinking_signature = Some(item.to_string());
                    }
                }
            }
            return;
        }
        match slot.kind {
            ResponseSlotKind::Thinking => {
                let summary_text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|summary| {
                        summary
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let content_text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|content| {
                        content
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let final_thinking = if summary_text.is_empty() {
                    content_text
                } else {
                    summary_text
                };
                if let Some(Content::Thinking(block)) = out
                    .content
                    .get_mut(slot.content_index)
                    .filter(|_| !final_thinking.is_empty())
                {
                    // Some endpoints supply reasoning only in the completed item.
                    // Forward that visible text through the same delta path.
                    let unseen = if block.thinking.trim().is_empty() {
                        Some(final_thinking.as_str())
                    } else {
                        final_thinking.strip_prefix(block.thinking.as_str())
                    };
                    let delta = unseen.filter(|text| !text.is_empty()).map(str::to_owned);
                    block.thinking = final_thinking;
                    if let Some(delta) = delta {
                        p.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: slot.content_index,
                            delta,
                            partial: Arc::new(out.clone()),
                        })
                        .await;
                    }
                }
                if let Some(Content::Thinking(block)) = out.content.get_mut(slot.content_index) {
                    if item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty())
                    {
                        block.thinking_signature = Some(item.to_string());
                    }
                    p.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: slot.content_index,
                        content: block.thinking.clone(),
                        partial: Arc::new(out.clone()),
                    })
                    .await;
                }
            }
            ResponseSlotKind::Text => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    let value = content
                        .iter()
                        .filter_map(|part| {
                            part.get("text")
                                .or_else(|| part.get("refusal"))
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !value.is_empty() {
                        if let Some(Content::Text(block)) = out.content.get_mut(slot.content_index)
                        {
                            block.text = value;
                        }
                    }
                }
                if let Some(Content::Text(block)) = out.content.get_mut(slot.content_index) {
                    if let Some(id) = &slot.item_id {
                        let phase = item.get("phase").and_then(Value::as_str);
                        block.text_signature = Some(encode_text_signature_v1(id, phase));
                    }
                    p.push(AssistantMessageEvent::TextEnd {
                        content_index: slot.content_index,
                        content: block.text.clone(),
                        partial: Arc::new(out.clone()),
                    })
                    .await;
                }
            }
            ResponseSlotKind::ToolCall => {
                if let Some(arguments) = item
                    .get("arguments")
                    .or_else(|| item.get("input"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    slot.arguments = arguments.to_string();
                }
                let call = ToolCall {
                    kind: ToolCallType,
                    id: compose_tool_call_id(slot.call_id.as_deref(), slot.item_id.as_deref()),
                    name: slot.name.clone(),
                    arguments: if slot.custom {
                        custom_tool_arguments(
                            slot.custom_input_property.as_deref().unwrap_or("input"),
                            &slot.arguments,
                        )
                    } else {
                        parse_streaming_json(Some(&slot.arguments))
                    },
                    thought_signature: None,
                    namespace: None,
                    recorded: None,
                };
                if let Some(content) = out.content.get_mut(slot.content_index) {
                    *content = Content::ToolCall(call.clone());
                }
                p.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: slot.content_index,
                    tool_call: call,
                    partial: Arc::new(out.clone()),
                })
                .await;
            }
        }
        slot.ended = true;
    }

    async fn reconcile_terminal_output(
        &mut self,
        items: &[Value],
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        for (output_index, item) in items.iter().enumerate() {
            let Some(kind) = response_slot_kind(item.get("type").and_then(Value::as_str)) else {
                continue;
            };
            let index = self
                .find_for_item(item, Some(output_index))
                .unwrap_or_else(|| self.reserve_index());
            if !self.slots.contains_key(&index) {
                self.add_item(index, item, out, p).await;
            } else if self.slots.get(&index).map(|slot| slot.kind) != Some(kind) {
                continue;
            }
            self.finish_item(index, item, out, p).await;
        }
    }

    async fn finish_open_slots(
        &mut self,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.flush_pending(out, p).await;
        let mut indexes: Vec<usize> = self
            .slots
            .iter()
            .filter_map(|(index, slot)| (!slot.ended).then_some(*index))
            .collect();
        indexes.sort_unstable();
        for index in indexes {
            let item = self
                .slots
                .get(&index)
                .map(|slot| {
                    json!({
                        "type": response_slot_type(slot.kind, slot.custom),
                        "id": slot.item_id,
                        "call_id": slot.call_id,
                        "name": slot.name,
                        if slot.custom { "input" } else { "arguments" }: slot.arguments,
                    })
                })
                .unwrap_or_else(|| json!({}));
            self.finish_item(index, &item, out, p).await;
        }
    }
}

fn response_slot_kind(kind: Option<&str>) -> Option<ResponseSlotKind> {
    match kind {
        Some("reasoning") => Some(ResponseSlotKind::Thinking),
        Some("message") => Some(ResponseSlotKind::Text),
        Some("function_call") | Some("custom_tool_call") => Some(ResponseSlotKind::ToolCall),
        _ => None,
    }
}

fn response_slot_type(kind: ResponseSlotKind, custom: bool) -> &'static str {
    match kind {
        ResponseSlotKind::Thinking => "reasoning",
        ResponseSlotKind::Text => "message",
        ResponseSlotKind::ToolCall if custom => "custom_tool_call",
        ResponseSlotKind::ToolCall => "function_call",
    }
}

/// Return the schema property used to carry a grammar tool's raw string input.
///
/// Responses represents grammar constrained tools as `custom` tools and their
/// input is a string, while pi's neutral ToolCall always stores an object. The
/// TypeScript implementation only enables this mapping for an object schema
/// with exactly one required string property. Invalid or incomplete schemas
/// are left as ordinary function tools by the request builder.
fn grammar_tool_input_property(tool: &Tool) -> Option<String> {
    let Some(ConstrainedSamplingConfig::Grammar { variants }) = tool.constrained_sampling.as_ref()
    else {
        return None;
    };
    if !variants.values().any(|value| !value.trim().is_empty()) {
        return None;
    }
    let schema = tool.parameters.as_value();
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let required = schema.get("required").and_then(Value::as_array)?;
    if required.len() != 1 {
        return None;
    }
    let property = required.first().and_then(Value::as_str)?;
    if property.is_empty() {
        return None;
    }
    if schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        != Some("string")
    {
        return None;
    }
    Some(property.to_string())
}

fn custom_tool_arguments(property: &str, input: &str) -> Value {
    let mut arguments = serde_json::Map::new();
    arguments.insert(property.to_string(), Value::String(input.to_string()));
    Value::Object(arguments)
}

fn supports_openai_grammar_tools(model: &Model) -> bool {
    matches!(
        model.compat.as_ref(),
        Some(StreamingProtocolCompat::OpenaiResponses(compat))
            if compat.supports_openai_grammar_tools.unwrap_or(false)
    )
}

fn grammar_tool_properties(model: &Model, tools: &[Tool]) -> HashMap<String, String> {
    if !supports_openai_grammar_tools(model) {
        return HashMap::new();
    }
    tools
        .iter()
        .filter_map(|tool| {
            grammar_tool_input_property(tool).map(|property| (tool.name.clone(), property))
        })
        .collect()
}

fn grammar_tool_spec(tool: &Tool, supported: bool) -> Option<(String, String, String)> {
    if !supported {
        return None;
    }
    let property = grammar_tool_input_property(tool)?;
    let ConstrainedSamplingConfig::Grammar { variants } = tool.constrained_sampling.as_ref()?
    else {
        return None;
    };
    if let Some(definition) = variants
        .get(&crate::types::GrammarFormat::OpenaiLark)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(("lark".to_string(), definition.clone(), property));
    }
    variants
        .get(&crate::types::GrammarFormat::OpenaiRegex)
        .filter(|value| !value.trim().is_empty())
        .map(|definition| ("regex".to_string(), definition.clone(), property))
}

fn compose_tool_call_id(call_id: Option<&str>, item_id: Option<&str>) -> String {
    match (
        call_id.filter(|value| !value.is_empty()),
        item_id.filter(|value| !value.is_empty()),
    ) {
        (Some(call_id), Some(item_id)) => format!("{call_id}|{item_id}"),
        (Some(call_id), None) => call_id.to_string(),
        (None, Some(item_id)) => item_id.to_string(),
        (None, None) => "call_unknown".to_string(),
    }
}

fn split_responses_tool_call_id(id: &str) -> (&str, Option<&str>) {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return (id, None);
    };

    // Rust stores the Responses pair as `call_id|item_id`, but provider call
    // ids are otherwise opaque and may themselves contain `|`. Only split the
    // spelling emitted by Responses (`call_*|fc_*`/`ctc_*`); all other values
    // remain one raw id and are normalized as a whole by the replay path.
    if call_id.starts_with("call_")
        && !item_id.contains('|')
        && (item_id.starts_with("fc_") || item_id.starts_with("ctc_"))
    {
        (call_id, Some(item_id))
    } else {
        (id, None)
    }
}

fn update_slot_metadata(slot: &mut ResponseSlot, item: &Value) {
    if let Some(value) = item.get("id").and_then(Value::as_str) {
        if !value.is_empty() {
            slot.item_id = Some(value.to_string());
        }
    }
    if let Some(value) = item.get("call_id").and_then(Value::as_str) {
        if !value.is_empty() {
            slot.call_id = Some(value.to_string());
        }
    }
    if let Some(value) = item.get("name").and_then(Value::as_str) {
        if !value.is_empty() {
            slot.name = value.to_string();
        }
    }
}

/// Responses message items carry a phase that distinguishes commentary from
/// the final answer. Surface that distinction in live partial messages so
/// consumers can stop rendering a final-answer block before the terminal
/// response event arrives. Keep an already-selected tool/error reason intact.
fn apply_message_phase(out: &mut AssistantMessage, item: &Value) {
    if item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        && out.stop_reason == StopReason::Pending
    {
        out.stop_reason = StopReason::Stop;
    }
}

fn resolve_api_key(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
) -> Option<String> {
    resolve_api_key_with_env(provider_key, opts, allow_env_api_key, || {
        std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
    })
}

fn resolve_api_key_with_env(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
    read_env_api_key: impl FnOnce() -> Option<String>,
) -> Option<String> {
    opts.api_key
        .clone()
        .or_else(|| provider_key.clone())
        .or_else(|| allow_env_api_key.then(read_env_api_key).flatten())
}

fn apply_api_key(
    headers: &mut std::collections::BTreeMap<String, String>,
    api_key: Option<String>,
) {
    if let Some(key) = api_key {
        headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
        headers.insert("authorization".into(), format!("Bearer {key}"));
    }
}

async fn run_stream(
    p: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    model: &Model,
    preparation: Result<PreparedRequest, String>,
    opts: &SimpleStreamOptions,
) {
    let mut out = AssistantMessage::empty(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    let PreparedRequest {
        headers,
        body,
        custom_tool_properties,
        url,
    } = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            emit_error(p, &mut out, error, false).await;
            return;
        }
    };
    let url = url.unwrap_or_else(|| responses_url(model));
    let timeout = opts.request_timeout();
    let signal = opts.signal.clone();
    let mut builder = http.post(&url).timeout(timeout);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let request = match super::request::PreparedJsonRequest::new(builder, &body) {
        Ok(request) => request,
        Err(error) => {
            emit_error(
                p,
                &mut out,
                format!("failed to encode request: {error}"),
                false,
            )
            .await;
            return;
        }
    };
    drop(body);
    let response = retry_provider_request(move || { let request=request.clone(); let signal=signal.clone(); async move {
        let response = tokio::select! { r=request.send()=>r?, _=signal.cancelled()=>return Err(AiError::Abort{message:"Request aborted".into()})};
        if response.status().is_success() { Ok(response) } else { let status=response.status().as_u16(); let msg=response.text().await.unwrap_or_default(); Err(AiError::Http{status:Some(status),message:msg}) }
    }}, opts.max_retries, opts.max_retry_delay, &opts.signal).await;
    let response = match response {
        Ok(r) => r,
        Err(e) => {
            emit_error(p, &mut out, e.to_string(), e.is_abort()).await;
            return;
        }
    };
    let mut events = SseEventStream::new(response, opts.signal.clone())
        .with_idle_timeout(opts.stream_idle_timeout());
    let mut state = ResponsesStreamState::with_tools(custom_tool_properties);
    // The stream contract exposes a pending assistant message immediately after
    // the HTTP response is accepted, before the first output item arrives.
    p.push(AssistantMessageEvent::Start {
        partial: Arc::new(out.clone()),
    })
    .await;
    loop {
        let Some(next) = events.next_event_before(state.flush_deadline()).await else {
            state.flush_pending(&mut out, p).await;
            continue;
        };
        match next {
            Ok(Some(ev)) => {
                // A few OpenAI-compatible gateways still terminate with the
                // Chat Completions-style `[DONE]` sentinel.  Treat it as a
                // normal terminal marker, but surface every other malformed
                // JSON frame instead of silently losing the real protocol
                // error and reporting only an unexpected EOF later.
                if ev.data.trim() == "[DONE]" {
                    state.terminal = true;
                    state.finish_open_slots(&mut out, p).await;
                    finish(p, out, model, None, None).await;
                    return;
                }
                if ev.data.trim().is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(error) => {
                        emit_error(
                            p,
                            &mut out,
                            format!(
                                "invalid OpenAI Responses SSE event: {error}; data={}",
                                ev.data
                            ),
                            false,
                        )
                        .await;
                        return;
                    }
                };
                if !state.mark_sequence(&value) {
                    // A few gateways replay an SSE frame after reconnecting.
                    // Sequence numbers make those duplicates harmless.
                    continue;
                }
                // Some Responses-compatible gateways omit the SSE `event:`
                // field and put the discriminator only in the JSON payload.
                // Preserve the wire event name when present, and use the
                // payload type as a fallback for those gateways.
                let kind = response_event_kind(ev.event.as_deref(), &value);
                match kind {
                    "response.created" => {
                        if let Some(response) = value.get("response") {
                            apply_response_metadata(&mut out, response);
                        }
                    }
                    "response.output_item.added" => {
                        if let Some(item) = value.get("item") {
                            let index = value
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .map(|value| value as usize)
                                .or_else(|| state.find_for_item(item, None))
                                .unwrap_or_else(|| state.reserve_index());
                            state.add_item(index, item, &mut out, p).await;
                        }
                    }
                    "response.content_part.added" => {
                        // Some compatible gateways omit output_item.added and
                        // announce only the first text content part.
                        let kind = value
                            .get("part")
                            .and_then(|part| part.get("type"))
                            .and_then(Value::as_str)
                            .map(|part| {
                                if part == "refusal" {
                                    ResponseSlotKind::Text
                                } else {
                                    ResponseSlotKind::Text
                                }
                            });
                        if let Some(kind) = kind {
                            state
                                .ensure_delta_slot(&value, kind, false, &mut out, p)
                                .await;
                        }
                    }
                    "response.output_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if let Some(index) = state
                                .ensure_delta_slot(
                                    &value,
                                    ResponseSlotKind::Text,
                                    false,
                                    &mut out,
                                    p,
                                )
                                .await
                            {
                                state.append_text(index, delta, &mut out, p, false).await;
                            }
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if let Some(index) = state
                                .ensure_delta_slot(
                                    &value,
                                    ResponseSlotKind::Thinking,
                                    false,
                                    &mut out,
                                    p,
                                )
                                .await
                            {
                                state.append_text(index, delta, &mut out, p, true).await;
                            }
                        }
                    }
                    "response.reasoning_summary_part.done" => {
                        if let Some(index) = state
                            .ensure_delta_slot(
                                &value,
                                ResponseSlotKind::Thinking,
                                false,
                                &mut out,
                                p,
                            )
                            .await
                        {
                            state.append_text(index, "\n\n", &mut out, p, true).await;
                        }
                    }
                    "response.refusal.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if let Some(index) = state
                                .ensure_delta_slot(
                                    &value,
                                    ResponseSlotKind::Text,
                                    false,
                                    &mut out,
                                    p,
                                )
                                .await
                            {
                                state.append_text(index, delta, &mut out, p, false).await;
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if let Some(index) = state
                                .ensure_delta_slot(
                                    &value,
                                    ResponseSlotKind::ToolCall,
                                    false,
                                    &mut out,
                                    p,
                                )
                                .await
                            {
                                state.append_tool_delta(index, delta, &mut out, p).await;
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        let Some(arguments) = value.get("arguments").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(index) = state
                            .ensure_delta_slot(
                                &value,
                                ResponseSlotKind::ToolCall,
                                false,
                                &mut out,
                                p,
                            )
                            .await
                        else {
                            continue;
                        };
                        let previous = state
                            .slots
                            .get(&index)
                            .map(|slot| slot.arguments.clone())
                            .unwrap_or_default();
                        if arguments.starts_with(&previous) {
                            let suffix = &arguments[previous.len()..];
                            if !suffix.is_empty() {
                                state.append_tool_delta(index, suffix, &mut out, p).await;
                            }
                        } else if let Some(slot) = state.slots.get_mut(&index) {
                            slot.arguments = arguments.to_string();
                            if let Some(Content::ToolCall(call)) =
                                out.content.get_mut(slot.content_index)
                            {
                                call.arguments = parse_streaming_json(Some(arguments));
                            }
                        }
                    }
                    "response.custom_tool_call_input.delta" => {
                        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(index) = state
                            .ensure_delta_slot(
                                &value,
                                ResponseSlotKind::ToolCall,
                                true,
                                &mut out,
                                p,
                            )
                            .await
                        else {
                            continue;
                        };
                        let current = state
                            .slots
                            .get(&index)
                            .map(|slot| slot.arguments.clone())
                            .unwrap_or_default();
                        let mut next = current;
                        next.push_str(delta);
                        state
                            .append_custom_tool_input(index, &next, false, &mut out, p)
                            .await;
                    }
                    "response.custom_tool_call_input.done" => {
                        let Some(index) = state
                            .ensure_delta_slot(
                                &value,
                                ResponseSlotKind::ToolCall,
                                true,
                                &mut out,
                                p,
                            )
                            .await
                        else {
                            continue;
                        };
                        let input = value
                            .get("input")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| state.slots.get(&index).map(|slot| slot.arguments.clone()))
                            .unwrap_or_default();
                        state
                            .append_custom_tool_input(index, &input, true, &mut out, p)
                            .await;
                    }
                    "response.output_item.done" => {
                        if let Some(item) = value.get("item") {
                            let index_hint = value
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .map(|value| value as usize);
                            let index = state
                                .find_for_item(item, index_hint)
                                .unwrap_or_else(|| state.reserve_index());
                            if !state.slots.contains_key(&index) {
                                state.add_item(index, item, &mut out, p).await;
                            }
                            state.finish_item(index, item, &mut out, p).await;
                        }
                    }
                    "response.completed" | "response.done" | "response.incomplete" => {
                        // OpenAI nests the terminal response under `response`,
                        // while a few Responses-compatible gateways put the
                        // response fields directly in the event payload.
                        let resp = value.get("response").unwrap_or(&value);
                        apply_response_metadata(&mut out, resp);
                        if let Some(items) = resp.get("output").and_then(Value::as_array) {
                            state.reconcile_terminal_output(items, &mut out, p).await;
                        }
                        state.terminal = true;
                        state.finish_open_slots(&mut out, p).await;
                        let status = response_terminal_status(kind, resp, &value);
                        let incomplete_reason = response_incomplete_reason(resp, &value);
                        finish(p, out, model, status, incomplete_reason).await;
                        return;
                    }
                    "response.failed" | "error" => {
                        let response = value.get("response").unwrap_or(&value);
                        apply_response_metadata(&mut out, response);
                        if let Some(status) = response.get("status").and_then(Value::as_str) {
                            out.raw_stop_reason = Some(status.to_string());
                        }
                        let msg = response_error_message(&value, response);
                        emit_error(p, &mut out, msg, false).await;
                        return;
                    }
                    _ => {}
                }
                p.commit_metadata(&out);
            }
            Ok(None) => {
                if !state.terminal {
                    emit_error(
                        p,
                        &mut out,
                        "OpenAI Responses stream ended before a terminal response event",
                        false,
                    )
                    .await;
                } else {
                    state.finish_open_slots(&mut out, p).await;
                    finish(p, out, model, None, None).await;
                }
                return;
            }
            Err(e) => {
                emit_error(p, &mut out, e.to_string(), e.is_abort()).await;
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedToolCall {
    /// The id that was persisted in the provider-neutral message.
    original_id: String,
    /// Canonical provider-neutral id (`call_id|item_id`, when both exist).
    canonical_id: String,
    call_id: String,
    item_id: Option<String>,
    name: String,
    arguments: Value,
    /// Set when this call belongs to a grammar-constrained custom tool.
    custom_input_property: Option<String>,
}

#[derive(Debug, Default)]
struct ToolCallIndex {
    calls: HashMap<String, PreparedToolCall>,
    /// Maps every id spelling seen in history to the canonical id.  This is
    /// deliberately built before serializing any messages, because old session
    /// files may use the item id (`fc_...`) as the tool result id.
    aliases: HashMap<String, String>,
}

impl ToolCallIndex {
    fn register(&mut self, call: PreparedToolCall) {
        let key = call.canonical_id.clone();
        let original = call.original_id.clone();
        let call_id = call.call_id.clone();
        let item_id = call.item_id.clone();
        // Keep the first complete invocation when malformed history repeats a
        // canonical id. Replacing it would make a later result resolve to a
        // different name/arguments than the call that was serialized first.
        self.calls.entry(key.clone()).or_insert(call);

        // Exact/full ids win.  Short aliases are only installed when no prior
        // call claimed them, so malformed duplicate history cannot redirect a
        // later tool result to a different invocation.
        self.aliases
            .entry(original.clone())
            .or_insert_with(|| key.clone());
        self.aliases
            .entry(key.clone())
            .or_insert_with(|| key.clone());
        self.aliases.entry(call_id).or_insert_with(|| key.clone());
        if let Some(item_id) = item_id {
            self.aliases.entry(item_id).or_insert_with(|| key.clone());
        }
    }

    fn resolve(&self, id: &str) -> Option<&PreparedToolCall> {
        self.aliases
            .get(id)
            .and_then(|key| self.calls.get(key))
            .or_else(|| self.calls.get(id))
    }
}

/// Build a valid Responses input from provider-neutral history.  In contrast
/// to chat-completions, Responses requires a `function_call` item before every
/// `function_call_output`; sending only the latter is the source of the
/// "No tool output found for function call ..." 400 error.
fn build_request(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> Value {
    let grammar_tool_properties = grammar_tool_properties(model, &ctx.tools);
    let mut index = ToolCallIndex::default();

    // First pass: normalize and index every replayable tool call.  The second
    // pass can then resolve results even when a legacy session stores only an
    // item id or uses a foreign provider's very long id.
    for message in &ctx.messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        if matches!(
            assistant.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            continue;
        }
        for content in &assistant.content {
            let Content::ToolCall(call) = content else {
                continue;
            };
            let mut prepared = prepare_tool_call_with_property(
                call,
                assistant,
                model,
                grammar_tool_properties.get(&call.name).map(String::as_str),
            );
            prepared.custom_input_property = grammar_tool_properties.get(&call.name).cloned();
            index.register(prepared);
        }
    }

    let mut input = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut completed: HashSet<String> = HashSet::new();
    let mut emitted_calls: HashSet<String> = HashSet::new();
    let mut emitted_call_ids: HashSet<String> = HashSet::new();
    let mut assistant_message_index = 0usize;

    for message in &ctx.messages {
        match message {
            Message::User(user) => {
                flush_pending_tool_results(&mut input, &mut pending, &mut completed, &index);
                let content = user_content_for_model(&user.content, model);
                if !content.is_null() {
                    input.push(json!({"role":"user","content":content}));
                }
            }
            Message::Assistant(assistant) => {
                let message_index = assistant_message_index;
                assistant_message_index = assistant_message_index.saturating_add(1);
                flush_pending_tool_results(&mut input, &mut pending, &mut completed, &index);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }

                // Replay opaque reasoning items before function calls. OpenAI
                // uses the reasoning item signature to validate a subsequent
                // fc_* function-call item; dropping it can produce a 400 even
                // when the call_id itself is correct.
                let can_replay_reasoning = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut text_block_index = 0usize;
                pending.clear();
                for content in &assistant.content {
                    match content {
                        Content::Thinking(thinking) => {
                            if can_replay_reasoning {
                                let Some(signature) = thinking.thinking_signature.as_deref() else {
                                    continue;
                                };
                                let Ok(item) = serde_json::from_str::<Value>(signature) else {
                                    continue;
                                };
                                if item.is_object()
                                    && item.get("type").and_then(Value::as_str) == Some("reasoning")
                                {
                                    input.push(item);
                                }
                            } else if !thinking.redacted && !thinking.thinking.trim().is_empty() {
                                // Reasoning signatures are model-bound. Preserve
                                // readable thinking when switching models by
                                // replaying it as an ordinary assistant message.
                                let fallback_id = if text_block_index == 0 {
                                    format!("msg_pi_{message_index}")
                                } else {
                                    format!("msg_pi_{message_index}_{text_block_index}")
                                };
                                text_block_index = text_block_index.saturating_add(1);
                                input
                                    .push(text_message_item_text(&thinking.thinking, &fallback_id));
                            }
                        }
                        Content::Text(text) => {
                            // Replay each assistant text block as a completed
                            // Responses message item. The item id is part of
                            // the provider metadata; retaining it avoids
                            // losing message identity on store:false
                            // multi-turn requests. Legacy plain-string
                            // signatures and the v1 JSON form are both
                            // accepted by `text_message_item`.
                            let fallback_id = if text_block_index == 0 {
                                format!("msg_pi_{message_index}")
                            } else {
                                format!("msg_pi_{message_index}_{text_block_index}")
                            };
                            text_block_index = text_block_index.saturating_add(1);
                            input.push(text_message_item(text, &fallback_id));
                        }
                        Content::ToolCall(call) => {
                            let Some(prepared) = index.resolve(&call.id) else {
                                continue;
                            };
                            // A malformed/legacy transcript can repeat the
                            // same canonical call (or call_id) in multiple
                            // assistant records. Responses treats those as
                            // duplicate items and rejects a later output, so
                            // serialize the first complete invocation only
                            // and resolve all aliases to it.
                            if !emitted_calls.insert(prepared.canonical_id.clone())
                                || !emitted_call_ids.insert(prepared.call_id.clone())
                            {
                                continue;
                            }
                            let mut item =
                                if let Some(property) = prepared.custom_input_property.as_deref() {
                                    let input = prepared
                                        .arguments
                                        .get(property)
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    json!({
                                        "type": "custom_tool_call",
                                        "call_id": prepared.call_id,
                                        "name": prepared.name,
                                        "input": input,
                                    })
                                } else {
                                    json!({
                                        "type": "function_call",
                                        "call_id": prepared.call_id,
                                        "name": prepared.name,
                                        "arguments": prepared.arguments.to_string(),
                                    })
                                };
                            if let Some(item_id) = &prepared.item_id {
                                item["id"] = Value::String(item_id.clone());
                            }
                            input.push(item);
                            pending.push(prepared.canonical_id.clone());
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let Some(prepared) = index.resolve(&result.tool_call_id) else {
                    // A result whose assistant call was dropped (for example an
                    // aborted turn) cannot be accepted by Responses.  Silently
                    // omit it rather than sending an orphaned output item.
                    continue;
                };
                // Only a result following the currently serialized assistant
                // call can be represented by a Responses output item. This
                // also filters malformed histories where a result precedes its
                // call or belongs to an aborted/dropped turn.
                let Some(position) = pending.iter().position(|key| key == &prepared.canonical_id)
                else {
                    continue;
                };
                pending.remove(position);
                let key = prepared.canonical_id.clone();
                if !completed.insert(key) {
                    // Retries or duplicated session records must not emit two
                    // outputs for one function call.
                    continue;
                }
                input.push(json!({
                    "type": if prepared.custom_input_property.is_some() {
                        "custom_tool_call_output"
                    } else {
                        "function_call_output"
                    },
                    "call_id": prepared.call_id,
                    "output": responses_tool_result_output(model, &result.content),
                }));
            }
        }
    }
    flush_pending_tool_results(&mut input, &mut pending, &mut completed, &index);

    // Responses requests are replayed statelessly from the provider-neutral
    // transcript. Disable server-side response storage so an omitted
    // `previous_response_id` cannot change that contract.
    let mut body = json!({"model":model.id,"input":input,"stream":true,"store":false});
    if let Some(s) = &ctx.system_prompt {
        body["instructions"] = Value::String(s.clone());
    }
    // OpenAI routes cached prefixes by this key; without it, consecutive turns
    // of one session can land on hosts that never saw the prefix and miss.
    if !matches!(opts.cache_retention, CacheRetention::None) {
        if let Some(session_id) = &opts.session_id {
            body["prompt_cache_key"] = Value::String(session_id.clone());
        }
    }
    let max = opts.max_tokens.unwrap_or(model.max_tokens);
    if max > 0 {
        // OpenAI rejects Responses requests with max_output_tokens < 16.
        body["max_output_tokens"] = Value::from(max.max(16));
    }
    if model.reasoning {
        if let Some(e) = reasoning_effort(model, opts.reasoning_level()) {
            body["reasoning"] = json!({"effort":e});
            if model.base_url.trim_end_matches('/') == "https://api.openai.com/v1" {
                body["reasoning"]["summary"] = json!("auto");
            }
        }
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(ctx.tools.iter().map(|t| {
            if let Some((syntax, definition, _property)) =
                grammar_tool_spec(t, supports_openai_grammar_tools(model))
            {
                json!({
                    "type": "custom",
                    "name": t.name,
                    "description": t.description,
                    "format": {
                        "type": "grammar",
                        "syntax": syntax,
                        "definition": definition,
                    },
                })
            } else {
                let mut tool = json!({"type":"function","name":t.name,"description":t.description,"parameters":t.parameters.as_value()});
                // OpenAI rejects `strict: true` unless every object node carries
                // `additionalProperties: false`, which `schemars` does not emit for
                // the built-in tools. Transform first and only advertise strict
                // when the transform succeeds, otherwise the request is a hard
                // 400 (`'additionalProperties' is required to be supplied and to
                // be false`).
                if supports_strict_mode(model) {
                    if let Some(strict_parameters) = strict_tool_parameters(t) {
                        tool["parameters"] = strict_parameters;
                        tool["strict"] = Value::Bool(true);
                    }
                }
                tool
            }
        }).collect());
    }
    if let Some(params) = &model.sampling_params {
        if let Value::Object(map) = &mut body {
            for (k, v) in params {
                map.insert(k.clone(), v.clone());
            }
        }
    }
    let xai = matches!(model.provider.as_str(), "xai" | "xai-oauth");
    if xai {
        if let Some(maximum) = body["max_output_tokens"].as_u64() {
            body["max_output_tokens"] = json!(maximum.min(64_000));
        }
    }
    let encrypted_reasoning = matches!(model.compat.as_ref(),
        Some(StreamingProtocolCompat::OpenaiResponses(compat))
            if compat.include_encrypted_reasoning == Some(true));
    if encrypted_reasoning || (xai && model.reasoning) {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if model.api == Api::OpenaiCodexResponses {
        shape_codex_request(&mut body);
    }
    body
}

/// ChatGPT's Codex endpoint speaks Responses SSE, but rejects the public
/// API's sampling/output limits and requires stateless encrypted reasoning.
fn shape_codex_request(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for name in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "top_k",
        "min_p",
        "presence_penalty",
        "repetition_penalty",
        "frequency_penalty",
        "stop",
        "prompt_cache_retention",
    ] {
        object.remove(name);
    }
    object.entry("instructions").or_insert_with(|| json!(""));
    object.insert("store".into(), json!(false));
    object.insert("stream".into(), json!(true));
    object.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.insert("summary".into(), json!("auto"));
    }
    if let Some(input) = object.get_mut("input").and_then(Value::as_array_mut) {
        input.retain(|item| item["type"] != "item_reference");
        for item in input {
            if item["type"] != "computer_call" {
                if let Some(item) = item.as_object_mut() {
                    item.remove("id");
                }
            }
        }
    }
}

/// Build the Responses output-message shape used when replaying an assistant
/// text block. Responses accepts a plain role/content message for new input,
/// but replayed assistant output needs the `type`, `status`, and bounded item
/// id metadata so the provider can correlate the item across turns.
fn text_message_item(text: &crate::types::TextContent, fallback_id: &str) -> Value {
    let (raw_id, phase) = parse_text_signature(text.text_signature.as_deref())
        .map(|(id, phase)| (id, phase))
        .unwrap_or_else(|| (fallback_id.to_string(), None));
    let id = bounded_message_id(&raw_id);
    let mut item = json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text.text,
            "annotations": []
        }],
        "status": "completed",
        "id": id,
    });
    if let Some(phase) = phase {
        item["phase"] = Value::String(phase);
    }
    item
}

fn text_message_item_text(text: &str, id: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": []
        }],
        "status": "completed",
        "id": bounded_message_id(id),
    })
}

/// Parse both the current `TextSignatureV1` JSON representation and the legacy
/// raw Responses message id. Invalid JSON intentionally falls back to the raw
/// value for compatibility with old session files.
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|value| !value.is_empty())?;
    if signature.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(signature) {
            if value.get("v").and_then(Value::as_u64) == Some(1) {
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        let phase = value
                            .get("phase")
                            .and_then(Value::as_str)
                            .filter(|phase| matches!(*phase, "commentary" | "final_answer"))
                            .map(str::to_string);
                        return Some((id.to_string(), phase));
                    }
                }
            }
        }
    }
    Some((signature.to_string(), None))
}

fn bounded_message_id(id: &str) -> String {
    if id.len() > 64 {
        format!("msg_{}", short_hash(id))
    } else {
        id.to_string()
    }
}

fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    let mut value = json!({"v": 1, "id": id});
    if let Some(phase) = phase.filter(|phase| matches!(*phase, "commentary" | "final_answer")) {
        value["phase"] = Value::String(phase.to_string());
    }
    value.to_string()
}

fn flush_pending_tool_results(
    input: &mut Vec<Value>,
    pending: &mut Vec<String>,
    completed: &mut HashSet<String>,
    index: &ToolCallIndex,
) {
    for key in pending.drain(..) {
        if !completed.insert(key.clone()) {
            continue;
        }
        let Some(call) = index.calls.get(&key) else {
            continue;
        };
        // Responses rejects a bare function_call at the next user turn.  A
        // deterministic synthetic output keeps the transcript structurally
        // valid when a tool was cancelled before producing a result.
        input.push(json!({
            "type": if call.custom_input_property.is_some() {
                "custom_tool_call_output"
            } else {
                "function_call_output"
            },
            "call_id": call.call_id,
            "output": "No result provided",
        }));
    }
}

fn prepare_tool_call_with_property(
    call: &ToolCall,
    source: &AssistantMessage,
    target: &Model,
    custom_input_property: Option<&str>,
) -> PreparedToolCall {
    let (raw_call_id, raw_item_id) = split_responses_tool_call_id(&call.id);
    let call_id = normalize_responses_id_part(raw_call_id, "call");
    let same_provider_different_model =
        source.provider == target.provider && source.api == target.api && source.model != target.id;
    let foreign = source.provider != target.provider || source.api != target.api;

    let item_id = raw_item_id.and_then(|raw| {
        if raw.is_empty() {
            return None;
        }
        // OpenAI tracks fc_* item ids together with reasoning items.  Replaying
        // an id from another model can therefore fail pairing validation; omit
        // it for same-provider model switches.  Foreign providers get a stable
        // short hash so their long/base64 ids remain within the Responses limit.
        if same_provider_different_model && raw.starts_with("fc_") {
            return None;
        }
        if foreign {
            return Some(format!("fc_{}", short_hash(raw)));
        }
        if custom_input_property.is_some() {
            return Some(normalize_responses_id_part(raw, "ctc"));
        }
        if same_provider_different_model || !raw.starts_with("fc_") {
            return None;
        }
        Some(normalize_responses_id_part(raw, "fc"))
    });
    let canonical_id = item_id
        .as_ref()
        .map(|item| format!("{call_id}|{item}"))
        .unwrap_or_else(|| call_id.clone());

    PreparedToolCall {
        original_id: call.id.clone(),
        canonical_id,
        call_id,
        item_id,
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        custom_input_property: custom_input_property.map(str::to_string),
    }
}

fn normalize_responses_id_part(raw: &str, fallback_prefix: &str) -> String {
    let mut out = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        format!("{fallback_prefix}_unknown")
    } else {
        out
    }
}

/// The reference implementation uses a compact base-36 two-word hash for
/// foreign Responses item ids.  Keep the same arithmetic so IDs remain stable
/// when a session moves between the Rust and TypeScript clients.
fn short_hash(value: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for ch in value.encode_utf16() {
        let ch = ch as u32;
        h1 = (h1 ^ ch).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ ch).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while value > 0 {
        out.push(digits[(value % 36) as usize] as char);
        value /= 36;
    }
    out.iter().rev().collect()
}

fn responses_tool_result_output(model: &Model, content: &[Content]) -> Value {
    let images: Vec<&ImageContent> = content
        .iter()
        .filter_map(|item| match item {
            Content::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    if images.is_empty() {
        let text = Content::text_only(content, "\n");
        return Value::String(if text.is_empty() {
            "(no tool output)".to_string()
        } else {
            text
        });
    }
    if !model.input.contains(&InputModality::Image) {
        // Match `transformMessages`' non-vision downgrade: preserve the image
        // position as a tool-specific text placeholder and collapse adjacent
        // images into one placeholder. Dropping the image here would make a
        // tool result's textual description look complete when it is not.
        const PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
        let mut parts = Vec::new();
        let mut previous_was_placeholder = false;
        for item in content {
            match item {
                Content::Image(_) => {
                    if !previous_was_placeholder {
                        parts.push(PLACEHOLDER.to_string());
                    }
                    previous_was_placeholder = true;
                }
                Content::Text(text) => {
                    parts.push(text.text.clone());
                    previous_was_placeholder = text.text == PLACEHOLDER;
                }
                _ => {
                    previous_was_placeholder = false;
                }
            }
        }
        return Value::String(if parts.is_empty() {
            "(no tool output)".to_string()
        } else {
            parts.join("\n")
        });
    }
    // Keep text and image blocks in their original order. Responses accepts an
    // array for `function_call_output.output`; flattening all text before all
    // images changes the meaning of results such as text -> image -> text.
    let mut output = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let flush_text = |output: &mut Vec<Value>, text_parts: &mut Vec<String>| {
        if text_parts.is_empty() {
            return;
        }
        let text = text_parts.join("\n");
        text_parts.clear();
        if !text.is_empty() {
            output.push(json!({"type":"input_text","text":text}));
        }
    };
    for item in content {
        match item {
            Content::Text(text) if !text.text.is_empty() => text_parts.push(text.text.clone()),
            Content::Image(image) => {
                flush_text(&mut output, &mut text_parts);
                output.push(json!({
                    "type": "input_image",
                    "detail": "auto",
                    "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                }));
            }
            _ => {}
        }
    }
    flush_text(&mut output, &mut text_parts);
    if output.is_empty() {
        return Value::String("(no tool output)".to_string());
    }
    Value::Array(output)
}

fn user_content_for_model(c: &UserContent, model: &Model) -> Value {
    match c {
        UserContent::Text(s) => Value::String(s.clone()),
        UserContent::Blocks(blocks) => {
            let supports_images = model.input.contains(&InputModality::Image);
            let mut content = Vec::with_capacity(blocks.len());
            let mut previous_image = false;
            for item in blocks {
                match item {
                    Content::Text(text) => {
                        content.push(json!({"type":"input_text","text":text.text}));
                        previous_image = false;
                    }
                    Content::Image(image) if supports_images => {
                        content.push(json!({
                            "type":"input_image",
                            "detail":"auto",
                            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                        }));
                        previous_image = false;
                    }
                    Content::Image(_) => {
                        if !previous_image {
                            content.push(json!({
                                "type":"input_text",
                                "text":"(image omitted: model does not support images)"
                            }));
                        }
                        previous_image = true;
                    }
                    _ => {
                        previous_image = false;
                    }
                }
            }
            if content.is_empty() {
                Value::Null
            } else {
                Value::Array(content)
            }
        }
    }
}

fn response_event_kind<'a>(event_name: Option<&'a str>, value: &'a Value) -> &'a str {
    let payload_type = value.get("type").and_then(Value::as_str);
    // Some gateways use a generic SSE event name (`message`) and put the
    // Responses discriminator in the JSON body. Prefer that discriminator when
    // it is recognizably a Responses event, while retaining explicit custom
    // event names for non-Responses payloads.
    let generic_event = event_name
        .filter(|name| !name.is_empty())
        .is_some_and(is_generic_sse_event_name);
    if generic_event && payload_type.is_some_and(is_responses_event_type) {
        return payload_type.unwrap_or("");
    }
    event_name
        .filter(|name| !name.is_empty())
        .or(payload_type)
        .unwrap_or("")
}

fn is_responses_event_type(name: &str) -> bool {
    name == "error" || name.starts_with("response.")
}

fn is_generic_sse_event_name(name: &str) -> bool {
    matches!(name, "message" | "data" | "event")
}

/// Resolve the terminal status across the official nested shape and the
/// top-level shape emitted by some OpenAI-compatible gateways.
fn response_terminal_status<'a>(
    kind: &str,
    response: &'a Value,
    event: &'a Value,
) -> Option<&'a str> {
    response
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| event.get("status").and_then(Value::as_str))
        .or_else(|| match kind {
            "response.incomplete" => Some("incomplete"),
            "response.completed" | "response.done" => Some("completed"),
            _ => None,
        })
}

/// Return the provider's specific reason for an `incomplete` response. OpenAI
/// normally nests this under `response.incomplete_details`, while compatible
/// gateways sometimes place it at the event root.
fn response_incomplete_reason<'a>(response: &'a Value, event: &'a Value) -> Option<&'a str> {
    response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
        })
}
fn responses_url(model: &Model) -> String {
    let b = model.base_url.trim_end_matches('/');
    if model.api == Api::OpenaiCodexResponses {
        if b.ends_with("/codex/responses") {
            b.to_owned()
        } else if b.ends_with("/codex") {
            format!("{b}/responses")
        } else {
            format!("{b}/codex/responses")
        }
    } else if b.ends_with("/v1") {
        format!("{b}/responses")
    } else {
        format!("{b}/v1/responses")
    }
}
fn reasoning_effort(model: &Model, l: ThinkingLevel) -> Option<String> {
    if matches!(model.compat.as_ref(), Some(StreamingProtocolCompat::OpenaiResponses(compat))
        if compat.supports_reasoning_effort == Some(false) || compat.omit_reasoning_effort == Some(true))
    {
        return None;
    }
    if let Some(map) = &model.thinking_level_map {
        if let Some(mapped) = map.get(&l) {
            return mapped.clone();
        }
    }
    match l {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal".into()),
        ThinkingLevel::Low => Some("low".into()),
        ThinkingLevel::Medium => Some("medium".into()),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high".into()),
    }
}
fn supports_strict_mode(model: &Model) -> bool {
    match model.compat.as_ref() {
        Some(StreamingProtocolCompat::OpenaiResponses(c)) => {
            c.supports_strict_mode.unwrap_or(false)
        }
        _ => false,
    }
}
fn apply_response_metadata(out: &mut AssistantMessage, resp: &Value) {
    if let Some(id) = resp.get("id").and_then(Value::as_str) {
        out.response_id = Some(id.into());
    }
    if let Some(m) = resp.get("model").and_then(Value::as_str) {
        out.response_model = Some(m.into());
    }
    if let Some(u) = resp.get("usage") {
        out.usage = parse_usage(u);
    }
}

fn response_error_message(event: &Value, response: &Value) -> String {
    let error = response
        .get("error")
        .or_else(|| event.get("error"))
        .or_else(|| {
            (response.get("code").is_some() || response.get("message").is_some())
                .then_some(response)
        });
    if let Some(error) = error {
        let code = error.get("code").and_then(Value::as_str);
        let message = error.get("message").and_then(Value::as_str);
        return match (code, message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (None, Some(message)) => message.to_string(),
            (Some(code), None) => code.to_string(),
            (None, None) => error.to_string(),
        };
    }
    if let Some(reason) = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        return format!("incomplete: {reason}");
    }
    response
        .get("message")
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Responses API error")
        .to_string()
}

fn parse_usage(v: &Value) -> Usage {
    let input = v.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let output = v.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
    let input_details = v.get("input_tokens_details");
    let cached = input_details
        .and_then(|x| x.get("cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_write = input_details
        .and_then(|x| x.get("cache_write_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning = v
        .get("output_tokens_details")
        .and_then(|x| x.get("reasoning_tokens"))
        .and_then(Value::as_i64);
    let total_tokens = v
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(input + output);
    Usage {
        input: (input - cached - cache_write).max(0),
        output,
        cache_read: cached,
        cache_write,
        cache_write_1h: None,
        reasoning,
        total_tokens,
        cost: Default::default(),
    }
}
async fn finish(
    p: &mut AssistantMessageEventStreamProducer,
    mut out: AssistantMessage,
    model: &Model,
    terminal_status: Option<&str>,
    _incomplete_reason: Option<&str>,
) {
    // The Responses API uses `incomplete` as the terminal status whenever the
    // model could not finish its response. The provider's
    // `incomplete_details.reason` is diagnostic metadata (and may be values
    // such as `content_filter`), rather than a separate transport error. Keep
    // the status in `raw_stop_reason` and map every incomplete response to the
    // provider-neutral length stop, matching the TypeScript implementation.
    out.raw_stop_reason = terminal_status.map(str::to_string);
    out.stop_reason = match terminal_status {
        Some("incomplete") => StopReason::Length,
        Some("failed" | "cancelled" | "canceled") => StopReason::Error,
        _ if out
            .content
            .iter()
            .any(|c| matches!(c, Content::ToolCall(_))) =>
        {
            StopReason::ToolUse
        }
        _ => StopReason::Stop,
    };
    out.usage.cost = calculate_cost(&model.cost, &out.usage);
    if matches!(out.stop_reason, StopReason::Error | StopReason::Aborted) {
        if out.error_message.is_none() {
            out.error_message = match terminal_status {
                Some(status) => Some(format!("OpenAI Responses finished with status {status}")),
                _ => Some("OpenAI Responses ended with an error".to_string()),
            };
        }
        p.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: out,
        })
        .await;
        return;
    }
    p.push(AssistantMessageEvent::Done {
        reason: match out.stop_reason {
            StopReason::ToolUse => DoneReason::ToolUse,
            StopReason::Length => DoneReason::Length,
            _ => DoneReason::Stop,
        },
        message: out,
    })
    .await;
}
async fn emit_error(
    p: &mut AssistantMessageEventStreamProducer,
    out: &mut AssistantMessage,
    msg: impl Into<String>,
    aborted: bool,
) {
    out.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    out.error_message = Some(msg.into());
    p.push(AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorReason::Aborted
        } else {
            ErrorReason::Error
        },
        error: out.clone(),
    })
    .await;
}
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;
    use crate::model::OpenaiResponsesCompat;
    use crate::types::{
        Api, AssistantMessage, Context, GrammarFormat, Message, Schema, StopReason, TextContent,
        TextContentType, ToolResultMessage, ToolResultRole,
    };
    use std::collections::BTreeMap;

    fn model() -> Model {
        Model::new(
            "gpt-test",
            "GPT Test",
            Api::OpenaiResponses,
            "openai",
            "https://example.test/v1",
        )
    }

    fn strict_model() -> Model {
        let mut model = model();
        model.compat = Some(StreamingProtocolCompat::OpenaiResponses(
            OpenaiResponsesCompat {
                supports_strict_mode: Some(true),
                ..Default::default()
            },
        ));
        model
    }

    #[tokio::test]
    async fn prepared_auth_failure_skips_body_and_keeps_terminal_error() {
        let model = model();
        let opts = SimpleStreamOptions::default();
        let prepared = prepare_request(
            &model,
            &Context::default(),
            &opts,
            &None,
            false,
            true,
            |_, _, _| panic!("missing credentials must not build the request"),
        );
        assert!(matches!(&prepared, Err(message) if message == "No API key"));
        let (mut producer, stream) = create_assistant_message_event_stream();
        run_stream(
            &mut producer,
            reqwest::Client::new(),
            &model,
            prepared,
            &opts,
        )
        .await;
        drop(producer);
        let message = stream.result().await.unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("No API key"));
    }

    #[test]
    fn prepared_request_preserves_header_auth_and_overrides() {
        let mut model = model();
        model.headers = Some(BTreeMap::from([(
            "Authorization".into(),
            "Bearer model-key".into(),
        )]));
        let opts = SimpleStreamOptions {
            headers: Some(BTreeMap::from([
                ("authorization".into(), "Bearer caller-key".into()),
                ("X-Api-Key".into(), "fixture-key".into()),
            ])),
            ..Default::default()
        };
        let prepared = prepare_request(
            &model,
            &Context::default(),
            &opts,
            &None,
            false,
            true,
            build_request,
        )
        .unwrap();
        assert_eq!(
            prepared.headers.get("authorization").map(String::as_str),
            Some("Bearer caller-key")
        );
        assert_eq!(
            prepared
                .headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("authorization"))
                .count(),
            1
        );
        assert_eq!(
            prepared.headers.get("X-Api-Key").map(String::as_str),
            Some("fixture-key")
        );
        assert_eq!(prepared.body["model"], "gpt-test");
    }

    #[test]
    fn prepared_body_and_grammar_mapping_own_the_original_context_snapshot() {
        for enabled in [false, true] {
            let model = if enabled { grammar_model() } else { model() };
            let opts = SimpleStreamOptions::default();
            let mut context = Context::new(vec![Message::User(crate::types::UserMessage::new(
                "original history ".repeat(4096),
                0,
            ))]);
            context.tools.push(grammar_tool());
            let expected = build_request(&model, &context, &opts);
            let prepared = prepare_request(
                &model,
                &context,
                &opts,
                &Some("fixture-key".into()),
                false,
                true,
                build_request,
            )
            .unwrap();
            context.messages.clear();
            context.tools[0].name = "replacement".into();
            context.tools[0].parameters = Schema::new(
                json!({"type":"object","properties":{"other":{"type":"string"}},"required":["other"]}),
            );
            drop(context);
            assert_eq!(prepared.body, expected);
            if enabled {
                assert_eq!(
                    prepared.custom_tool_properties,
                    HashMap::from([("run_grammar".into(), "payload".into())])
                );
            } else {
                assert!(prepared.custom_tool_properties.is_empty());
            }
        }
    }

    /// Regression: `strict: true` used to be advertised alongside the raw
    /// `schemars` schema, which has no `additionalProperties: false`. OpenAI
    /// rejected that with
    ///
    /// ```text
    /// Invalid schema for function 'read': In context=(), 'additionalProperties'
    /// is required to be supplied and to be false.
    ///   "param": "tools[0].parameters"
    /// ```
    ///
    /// which failed every request that carried the built-in tools.
    #[test]
    fn strict_mode_transforms_the_schema_before_advertising_strict() {
        use crate::types::{ConstrainedSamplingConfig, ConstrainedStrictness};

        // Mirrors the built-in `read` tool: schemars output, no
        // `additionalProperties`, only `path` required.
        let opting_in = Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: Schema::new(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": ["integer", "null"]}
                },
                "required": ["path"]
            })),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        };
        // Precondition: the untransformed schema is exactly what OpenAI rejects.
        assert!(
            opting_in
                .parameters
                .as_value()
                .get("additionalProperties")
                .is_none(),
            "precondition: schemars does not emit additionalProperties"
        );

        // Mirrors `find` / `grep` / `ls` / `powershell`, which opt out.
        let opting_out = Tool {
            name: "ls".into(),
            description: "List a directory".into(),
            parameters: Schema::new(json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": []
            })),
            constrained_sampling: None,
        };

        let context = Context {
            system_prompt: None,
            messages: Vec::new(),
            tools: vec![opting_in, opting_out],
        };
        let body = build_request(&strict_model(), &context, &SimpleStreamOptions::default());
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);

        // Opted in: strict is advertised *and* the schema was transformed.
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[0]["strict"], true);
        assert_eq!(tools[0]["parameters"]["additionalProperties"], false);
        let mut required: Vec<&str> = tools[0]["parameters"]["required"]
            .as_array()
            .expect("required array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(
            required,
            vec!["offset", "path"],
            "strict mode requires every property to be listed"
        );

        // Opted out: no `strict` key, so the raw schema is sent unchanged.
        assert_eq!(tools[1]["name"], "ls");
        assert!(
            tools[1].get("strict").is_none(),
            "an opted-out tool must not advertise strict: {}",
            tools[1]
        );
        assert!(tools[1]["parameters"].get("additionalProperties").is_none());
    }

    #[test]
    fn isolated_custom_provider_never_overwrites_auth_with_openai_env_key() {
        let opts = SimpleStreamOptions::default();
        let env_read = std::cell::Cell::new(false);
        let resolved = resolve_api_key_with_env(&None, &opts, false, || {
            env_read.set(true);
            Some("official-openai-key".into())
        });
        let mut headers = BTreeMap::from([("authorization".into(), "Bearer gateway-key".into())]);
        apply_api_key(&mut headers, resolved);

        assert!(
            !env_read.get(),
            "isolated providers must not read OpenAI env auth"
        );
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer gateway-key")
        );
        assert!(headers
            .values()
            .all(|value| value != "Bearer official-openai-key"));
    }

    #[test]
    fn only_exact_openai_provider_id_inherits_the_environment_key() {
        let official = OpenAiResponsesProvider::with_models(
            "openai",
            None,
            reqwest::Client::new(),
            Vec::new(),
        );
        let case_variant = OpenAiResponsesProvider::with_models(
            "OpenAI",
            None,
            reqwest::Client::new(),
            Vec::new(),
        );

        assert!(official.allow_env_api_key);
        assert!(!case_variant.allow_env_api_key);
    }

    fn assistant_with_call(id: &str) -> AssistantMessage {
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![Content::tool_call(id, "read", json!({"path":"README.md"}))];
        assistant
    }

    fn tool_result(id: &str) -> Message {
        ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 2,
        }
        .into()
    }

    fn grammar_tool() -> Tool {
        let mut variants = BTreeMap::new();
        variants.insert(GrammarFormat::OpenaiLark, "start: /.+/".to_string());
        Tool {
            name: "run_grammar".into(),
            description: "run grammar input".into(),
            parameters: Schema::new(json!({
                "type": "object",
                "properties": {"payload": {"type": "string"}},
                "required": ["payload"]
            })),
            constrained_sampling: Some(ConstrainedSamplingConfig::Grammar { variants }),
        }
    }

    fn grammar_model() -> Model {
        let mut model = model();
        model.compat = Some(StreamingProtocolCompat::OpenaiResponses(
            OpenaiResponsesCompat {
                supports_openai_grammar_tools: Some(true),
                ..Default::default()
            },
        ));
        model
    }

    /// The Responses provider carried the same per-delta full-message clone as
    /// the other two, so it had the same quadratic cost. Batching is shared
    /// policy (`providers::DELTA_FLUSH_BYTES`), and the item boundary flush in
    /// `finish_item` must still deliver the tail.
    #[tokio::test]
    async fn text_deltas_are_batched_without_losing_the_tail() {
        let count = 500usize;
        let (mut producer, stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(
                0,
                &json!({ "type": "message", "id": "msg_1" }),
                &mut output,
                &mut producer,
            )
            .await;
        for _ in 0..count {
            state
                .append_text(0, "x", &mut output, &mut producer, false)
                .await;
        }
        state
            .finish_item(
                0,
                &json!({ "type": "message", "id": "msg_1" }),
                &mut output,
                &mut producer,
            )
            .await;

        drop(producer);
        let mut receiver = stream;
        let mut tags: Vec<&'static str> = Vec::new();
        let mut joined = String::new();
        while let Some(event) = receiver.next().await {
            tags.push(event.type_tag());
            if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
                joined.push_str(delta);
            }
        }

        let deltas = tags.iter().filter(|tag| **tag == "text_delta").count();
        assert!(
            deltas <= count / DELTA_FLUSH_BYTES + 5,
            "{count} one-byte deltas must coalesce into ~{}/{DELTA_FLUSH_BYTES} events, got {deltas}",
            count
        );
        assert_eq!(joined.len(), count, "no byte may be dropped");
        // The item boundary flush ran before `text_end`.
        let end_at = tags
            .iter()
            .position(|tag| *tag == "text_end")
            .expect("a text_end was emitted");
        let last_delta_at = tags
            .iter()
            .rposition(|tag| *tag == "text_delta")
            .expect("a text_delta was emitted");
        assert!(
            last_delta_at < end_at,
            "the batched tail must be flushed before text_end: {tags:?}"
        );
    }

    #[tokio::test]
    async fn custom_tool_stream_wraps_and_escapes_raw_input_once() {
        let tool = grammar_tool();
        let (mut producer, stream) = create_assistant_message_event_stream();
        let mut state =
            ResponsesStreamState::with_tools(grammar_tool_properties(&grammar_model(), &[tool]));
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(
                0,
                &json!({
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_1",
                    "name": "run_grammar"
                }),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .append_custom_tool_input(0, "a\"", false, &mut output, &mut producer)
            .await;
        state
            .append_custom_tool_input(0, "a\"\nb", true, &mut output, &mut producer)
            .await;
        state
            .finish_item(
                0,
                &json!({
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_1",
                    "name": "run_grammar",
                    "input": "a\"\nb"
                }),
                &mut output,
                &mut producer,
            )
            .await;

        drop(producer);
        let mut receiver = stream;
        let mut deltas = Vec::new();
        while let Some(event) = receiver.next().await {
            if let AssistantMessageEvent::ToolCallDelta { delta, .. } = event {
                deltas.push(delta);
            }
        }
        assert_eq!(deltas.concat(), "{\"payload\":\"a\\\"\\nb\"}");
        assert!(matches!(
            &output.content[0],
            Content::ToolCall(call) if call.arguments == json!({"payload": "a\"\nb"})
        ));
        assert!(state.slots.get(&0).is_some_and(|slot| slot.ended));
    }

    #[test]
    fn grammar_tools_use_custom_shapes_in_request_replay() {
        let tool = grammar_tool();
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![Content::tool_call(
            "call_1|ctc_1",
            "run_grammar",
            json!({"payload": "a\"\nb"}),
        )];
        let result = ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call_1|ctc_1".into(),
            tool_name: "run_grammar".into(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 2,
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant)), result.into()],
            tools: vec![tool],
        };
        let body = build_request(&grammar_model(), &context, &SimpleStreamOptions::default());
        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["input"][0]["type"], "custom_tool_call");
        assert_eq!(body["input"][0]["input"], "a\"\nb");
        assert_eq!(body["input"][1]["type"], "custom_tool_call_output");
    }

    #[tokio::test]
    async fn output_item_done_keeps_streamed_function_arguments_when_wire_value_is_empty() {
        let (mut producer, stream) = create_assistant_message_event_stream();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        let mut state = ResponsesStreamState::default();
        state
            .add_item(
                0,
                &json!({
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read"
                }),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .append_tool_delta(0, "{\"path\":\"README.md\"}", &mut output, &mut producer)
            .await;
        state
            .finish_item(
                0,
                &json!({
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": ""
                }),
                &mut output,
                &mut producer,
            )
            .await;

        drop(stream);
        assert_eq!(
            output.content[0],
            Content::tool_call("call_1|fc_1", "read", json!({"path": "README.md"}))
        );
    }

    #[tokio::test]
    async fn final_answer_phase_is_visible_in_live_partial_message() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        let mut state = ResponsesStreamState::default();
        state
            .add_item(
                0,
                &json!({
                    "type": "message",
                    "id": "msg_1",
                    "phase": "final_answer"
                }),
                &mut output,
                &mut producer,
            )
            .await;
        assert_eq!(output.stop_reason, StopReason::Stop);
    }

    #[test]
    fn grammar_compat_uses_catalog_field_name() {
        let compat = OpenaiResponsesCompat {
            supports_openai_grammar_tools: Some(true),
            ..Default::default()
        };
        let encoded = serde_json::to_value(compat).unwrap();
        assert_eq!(encoded["supportsOpenAIGrammarTools"], true);
        assert!(encoded.get("supportsOpenaiGrammarTools").is_none());
    }

    #[test]
    fn preserves_responses_call_and_item_ids_across_tool_roundtrip() {
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        assistant.content = vec![Content::tool_call(
            "call_123|fc_456",
            "read",
            json!({"path":"README.md"}),
        )];
        let tool_result = ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call_123|fc_456".into(),
            tool_name: "read".into(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 2,
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant)), tool_result.into()],
            tools: Vec::new(),
        };

        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "call_123");
        assert_eq!(body["input"][0]["id"], "fc_456");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], "call_123");
    }

    #[test]
    fn session_id_becomes_the_prompt_cache_key_unless_caching_is_off() {
        let context = Context {
            system_prompt: Some("system".into()),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let key = |opts: &SimpleStreamOptions| {
            let body = build_request(&model(), &context, opts);
            body.get("prompt_cache_key").cloned()
        };
        let mut opts = SimpleStreamOptions::default();
        assert_eq!(key(&opts), None);
        opts.session_id = Some("sess-1".into());
        assert_eq!(key(&opts), Some(json!("sess-1")));
        opts.cache_retention = CacheRetention::None;
        assert_eq!(key(&opts), None);
    }

    #[test]
    fn opaque_tool_call_id_with_pipe_is_normalized_as_one_id() {
        let opaque_id = "vendor|call";
        assert_eq!(split_responses_tool_call_id(opaque_id), (opaque_id, None));
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant_with_call(opaque_id))),
            tool_result(opaque_id),
        ]);

        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "vendor_call");
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "vendor_call");
    }

    #[test]
    fn item_id_only_tool_result_resolves_to_the_original_call_id() {
        let assistant = assistant_with_call("call_123|fc_456");
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant)),
            tool_result("fc_456"),
        ]);

        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], "call_123");
    }

    #[test]
    fn foreign_item_id_is_stable_and_bounded() {
        let raw_item_id = format!("fc_{}", "x".repeat(400));
        let mut source = AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "claude", 1);
        source.content = vec![Content::tool_call(
            &format!("call_original|{raw_item_id}"),
            "read",
            json!({}),
        )];
        let prepared = match &source.content[0] {
            Content::ToolCall(call) => {
                prepare_tool_call_with_property(call, &source, &model(), None)
            }
            _ => unreachable!(),
        };
        let item_id = prepared.item_id.expect("foreign calls receive an item id");
        assert!(item_id.starts_with("fc_"));
        assert!(item_id.len() <= 64);
        assert_eq!(item_id, format!("fc_{}", short_hash(&raw_item_id)));
    }

    #[test]
    fn missing_tool_result_gets_a_synthetic_output_before_next_user_turn() {
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant_with_call("call_123|fc_456"))),
            Message::User(crate::types::UserMessage::new("continue", 3)),
        ]);
        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "No result provided");
        assert_eq!(body["input"][2]["role"], "user");
    }

    #[test]
    fn aborted_assistant_tool_result_is_omitted() {
        let mut assistant = assistant_with_call("call_aborted|fc_aborted");
        assistant.stop_reason = StopReason::Aborted;
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant)),
            tool_result("call_aborted|fc_aborted"),
        ]);
        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        assert!(body["input"]
            .as_array()
            .is_some_and(|items| items.is_empty()));
    }

    #[test]
    fn duplicate_tool_results_emit_only_one_output() {
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant_with_call("call_123|fc_456"))),
            tool_result("call_123|fc_456"),
            tool_result("fc_456"),
        ]);
        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        let outputs = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count();
        assert_eq!(outputs, 1);
    }

    #[test]
    fn legacy_fc_only_id_is_replayed_as_a_call_before_its_output() {
        let legacy_id = "fc_Qgl0hBWAvSkQ9YXljCq7WLxS";
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant_with_call(legacy_id))),
            tool_result(legacy_id),
        ]);
        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], legacy_id);
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], legacy_id);
    }

    #[test]
    fn duplicate_canonical_calls_and_outputs_are_serialized_once() {
        let mut assistant = assistant_with_call("call_duplicate|fc_duplicate");
        let duplicate = match &assistant.content[0] {
            Content::ToolCall(call) => call.clone(),
            _ => unreachable!(),
        };
        assistant.content.push(Content::ToolCall(duplicate));
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant)),
            tool_result("call_duplicate|fc_duplicate"),
            tool_result("fc_duplicate"),
        ]);
        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call")
                .count(),
            1
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            1
        );
    }

    #[test]
    fn assistant_text_signature_is_replayed_as_a_completed_message_item() {
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![Content::Text(TextContent {
            kind: TextContentType,
            text: "answer".to_string(),
            text_signature: Some(
                json!({"v":1,"id":"msg_provider","phase":"final_answer"}).to_string(),
            ),
        })];
        let body = build_request(
            &model(),
            &Context::new(vec![Message::Assistant(Box::new(assistant))]),
            &SimpleStreamOptions::default(),
        );
        let item = &body["input"][0];
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["id"], "msg_provider");
        assert_eq!(item["phase"], "final_answer");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "answer");
    }

    #[test]
    fn unsupported_user_images_are_preserved_as_a_placeholder() {
        let user = crate::types::UserMessage::new(
            UserContent::Blocks(vec![
                Content::text("before"),
                Content::Image(ImageContent {
                    kind: Default::default(),
                    data: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                }),
                Content::Image(ImageContent {
                    kind: Default::default(),
                    data: "d29ybGQ=".into(),
                    mime_type: "image/png".into(),
                }),
                Content::text("after"),
            ]),
            3,
        );
        let body = build_request(
            &model(),
            &Context::new(vec![Message::User(user)]),
            &SimpleStreamOptions::default(),
        );
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(
            content[1]["text"],
            "(image omitted: model does not support images)"
        );
    }

    #[test]
    fn unsupported_tool_images_are_preserved_with_tool_placeholder() {
        let content = vec![
            Content::text("before"),
            Content::Image(ImageContent {
                kind: Default::default(),
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }),
            Content::Image(ImageContent {
                kind: Default::default(),
                data: "d29ybGQ=".into(),
                mime_type: "image/png".into(),
            }),
            Content::text("after"),
        ];
        assert_eq!(
            responses_tool_result_output(&model(), &content),
            json!("before\n(tool image omitted: model does not support images)\nafter")
        );
    }

    #[test]
    fn vision_tool_result_preserves_text_and_image_order() {
        let mut vision_model = model();
        vision_model.input.push(InputModality::Image);
        let content = vec![
            Content::text("before"),
            Content::Image(ImageContent {
                kind: Default::default(),
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }),
            Content::text("after"),
        ];

        let output = responses_tool_result_output(&vision_model, &content);
        let blocks = output.as_array().expect("vision output should be blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "input_text");
        assert_eq!(blocks[0]["text"], "before");
        assert_eq!(blocks[1]["type"], "input_image");
        assert_eq!(blocks[1]["image_url"], "data:image/png;base64,aGVsbG8=");
        assert_eq!(blocks[2]["type"], "input_text");
        assert_eq!(blocks[2]["text"], "after");
    }

    #[test]
    fn plain_tool_call_ids_remain_backward_compatible() {
        assert_eq!(split_responses_tool_call_id("call_123"), ("call_123", None));
    }

    #[test]
    fn responses_clamps_small_max_output_tokens_to_api_minimum() {
        let mut options = SimpleStreamOptions::default();
        options.max_tokens = Some(1);
        let body = build_request(&model(), &Context::new(Vec::new()), &options);
        assert_eq!(body["max_output_tokens"], 16);
    }

    #[test]
    fn response_event_kind_uses_payload_discriminator_when_event_name_is_missing() {
        let value = json!({"type":"response.done","response":{}});
        assert_eq!(response_event_kind(None, &value), "response.done");
    }

    #[test]
    fn response_event_kind_prefers_payload_for_generic_sse_event_name() {
        let value = json!({"type":"response.output_text.delta","delta":"hello"});
        assert_eq!(
            response_event_kind(Some("message"), &value),
            "response.output_text.delta"
        );
    }

    #[test]
    fn response_event_kind_falls_back_to_sse_event_name() {
        let value = json!({"delta":"hello"});
        assert_eq!(
            response_event_kind(Some("response.output_text.delta"), &value),
            "response.output_text.delta"
        );
    }

    #[test]
    fn response_event_kind_keeps_event_name_when_discriminators_disagree() {
        let value = json!({"type":"response.other"});
        assert_eq!(
            response_event_kind(Some("response.output_text.delta"), &value),
            "response.output_text.delta"
        );
    }

    #[test]
    fn response_terminal_status_supports_nested_and_top_level_shapes() {
        let nested_event = json!({
            "type": "response.done",
            "response": {"status": "incomplete"}
        });
        assert_eq!(
            response_terminal_status(
                "response.done",
                nested_event.get("response").unwrap(),
                &nested_event
            ),
            Some("incomplete")
        );

        let top_level_event = json!({"type":"response.incomplete"});
        assert_eq!(
            response_terminal_status("response.incomplete", &top_level_event, &top_level_event),
            Some("incomplete")
        );
    }

    #[test]
    fn nested_response_failure_preserves_code_and_message() {
        let event = json!({
            "type": "response.failed",
            "response": {
                "status": "failed",
                "error": {"code": "server_error", "message": "upstream failed"}
            }
        });
        let response = event.get("response").unwrap();
        assert_eq!(
            response_error_message(&event, response),
            "server_error: upstream failed"
        );
    }

    #[test]
    fn top_level_error_event_preserves_code_and_message() {
        let event = json!({
            "type": "error",
            "code": "rate_limit",
            "message": "try again later"
        });
        assert_eq!(
            response_error_message(&event, &event),
            "rate_limit: try again later"
        );
    }

    #[test]
    fn parse_usage_accounts_for_cache_write_and_reasoning_tokens() {
        let usage = parse_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "total_tokens": 120,
            "input_tokens_details": {
                "cached_tokens": 30,
                "cache_write_tokens": 10
            },
            "output_tokens_details": {"reasoning_tokens": 7}
        }));
        assert_eq!(usage.input, 60);
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.reasoning, Some(7));
        assert_eq!(usage.total_tokens, 120);
    }

    #[tokio::test]
    async fn idless_text_deltas_reuse_the_open_text_slot() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);

        let first = state
            .ensure_delta_slot(
                &json!({"delta":"a"}),
                ResponseSlotKind::Text,
                false,
                &mut output,
                &mut producer,
            )
            .await
            .unwrap();
        state
            .append_text(first, "a", &mut output, &mut producer, false)
            .await;
        let second = state
            .ensure_delta_slot(
                &json!({"delta":"b"}),
                ResponseSlotKind::Text,
                false,
                &mut output,
                &mut producer,
            )
            .await
            .unwrap();
        state
            .append_text(second, "b", &mut output, &mut producer, false)
            .await;

        assert_eq!(first, second);
        assert_eq!(output.content.len(), 1);
        assert!(matches!(&output.content[0], Content::Text(text) if text.text == "ab"));
    }

    #[test]
    fn constant_sequence_numbers_never_drop_repeated_deltas() {
        let mut state = ResponsesStreamState::default();
        let a = json!({
            "sequence_number": 0,
            "type": "response.output_text.delta",
            "delta": "a"
        });
        let b = json!({
            "sequence_number": 0,
            "type": "response.output_text.delta",
            "delta": "b"
        });
        assert!(state.mark_sequence(&a));
        assert!(state.mark_sequence(&a));
        assert!(state.mark_sequence(&b));
        assert!(state.mark_sequence(&a));
    }

    #[test]
    fn reliable_sequence_numbers_suppress_exact_replays() {
        let mut state = ResponsesStreamState::default();
        let first = json!({
            "sequence_number": 0,
            "type": "response.output_text.delta",
            "delta": "a"
        });
        let second = json!({
            "sequence_number": 1,
            "type": "response.output_text.delta",
            "delta": "b"
        });
        assert!(state.mark_sequence(&first));
        assert!(state.mark_sequence(&second));
        assert!(!state.mark_sequence(&first));
    }

    #[tokio::test]
    async fn unknown_identified_delta_does_not_corrupt_another_open_slot() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(
                0,
                &json!({"type":"message","id":"msg_a"}),
                &mut output,
                &mut producer,
            )
            .await;
        let index = state
            .ensure_delta_slot(
                &json!({"item_id":"msg_b","delta":"b"}),
                ResponseSlotKind::Text,
                false,
                &mut output,
                &mut producer,
            )
            .await
            .unwrap();
        state
            .append_text(index, "b", &mut output, &mut producer, false)
            .await;
        assert_ne!(index, 0);
        assert!(matches!(&output.content[0], Content::Text(text) if text.text.is_empty()));
        assert!(matches!(&output.content[index], Content::Text(text) if text.text == "b"));
    }

    #[tokio::test]
    async fn sparse_output_index_routes_delta_to_its_wire_slot() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);

        let index = state
            .ensure_delta_slot(
                &json!({"output_index": 3, "item_id": "msg_sparse"}),
                ResponseSlotKind::Text,
                false,
                &mut output,
                &mut producer,
            )
            .await
            .unwrap();
        assert_eq!(index, 3);

        state
            .append_text(index, "kept", &mut output, &mut producer, false)
            .await;
        assert!(matches!(&output.content[0], Content::Text(text) if text.text == "kept"));
    }

    #[tokio::test]
    async fn sparse_output_index_routes_function_arguments_to_its_wire_slot() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);

        let index = state
            .ensure_delta_slot(
                &json!({
                    "output_index": 5,
                    "item_id": "fc_sparse",
                    "call_id": "call_sparse",
                    "name": "read"
                }),
                ResponseSlotKind::ToolCall,
                false,
                &mut output,
                &mut producer,
            )
            .await
            .unwrap();
        assert_eq!(index, 5);

        state
            .append_tool_delta(
                index,
                "{\"path\":\"README.md\"}",
                &mut output,
                &mut producer,
            )
            .await;
        assert!(matches!(
            &output.content[0],
            Content::ToolCall(call)
                if call.id == "call_sparse|fc_sparse"
                    && call.arguments == json!({"path":"README.md"})
        ));
    }

    #[tokio::test]
    async fn terminal_items_without_ids_are_aligned_by_output_index() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(0, &json!({"type":"message"}), &mut output, &mut producer)
            .await;
        state
            .add_item(1, &json!({"type":"message"}), &mut output, &mut producer)
            .await;
        state
            .reconcile_terminal_output(
                &[
                    json!({"type":"message","content":[{"type":"output_text","text":"first"}]}),
                    json!({"type":"message","content":[{"type":"output_text","text":"second"}]}),
                ],
                &mut output,
                &mut producer,
            )
            .await;
        assert!(matches!(&output.content[0], Content::Text(text) if text.text == "first"));
        assert!(matches!(&output.content[1], Content::Text(text) if text.text == "second"));
    }

    #[tokio::test]
    async fn terminal_reasoning_item_backfills_a_missing_encrypted_signature() {
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(
                0,
                &json!({"type":"reasoning","id":"rs_1"}),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .finish_item(
                0,
                &json!({"type":"reasoning","id":"rs_1","summary":[{"text":"think"}]}),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .reconcile_terminal_output(
                &[json!({
                    "type":"reasoning",
                    "id":"rs_1",
                    "summary":[{"text":"think"}],
                    "encrypted_content":"opaque"
                })],
                &mut output,
                &mut producer,
            )
            .await;
        let Content::Thinking(thinking) = &output.content[0] else {
            panic!("expected thinking content");
        };
        assert!(thinking
            .thinking_signature
            .as_deref()
            .is_some_and(|value| value.contains("encrypted_content")));
    }

    #[tokio::test]
    async fn output_item_done_and_terminal_output_emit_one_tool_call_end() {
        let (mut producer, stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        let item = json!({
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_1",
            "name":"read"
        });
        state.add_item(0, &item, &mut output, &mut producer).await;
        state
            .append_tool_delta(0, "{\"path\":\"a\"}", &mut output, &mut producer)
            .await;
        state
            .finish_item(
                0,
                &json!({
                    "type":"function_call",
                    "id":"fc_1",
                    "call_id":"call_1",
                    "name":"read",
                    "arguments":"{\"path\":\"a\"}"
                }),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .reconcile_terminal_output(
                &[json!({
                    "type":"function_call",
                    "id":"fc_1",
                    "call_id":"call_1",
                    "name":"read",
                    "arguments":"{\"path\":\"a\"}"
                })],
                &mut output,
                &mut producer,
            )
            .await;

        drop(stream);
        // The slot is ended after the first item; terminal reconciliation must
        // not append a second tool call or emit another end event.
        assert_eq!(output.content.len(), 1);
        assert!(state.slots.get(&0).is_some_and(|slot| slot.ended));
    }

    #[test]
    fn reasoning_signature_is_not_replayed_across_models() {
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-old", 1);
        assistant.content = vec![Content::Thinking(crate::types::ThinkingContent {
            kind: Default::default(),
            thinking: String::new(),
            thinking_signature: Some(
                json!({"type":"reasoning","id":"rs_old","encrypted_content":"opaque"}).to_string(),
            ),
            redacted: false,
        })];
        let body = build_request(
            &model(),
            &Context::new(vec![Message::Assistant(Box::new(assistant))]),
            &SimpleStreamOptions::default(),
        );
        assert!(body["input"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["type"] != "reasoning")));
    }

    #[test]
    fn visible_thinking_is_replayed_as_text_across_models() {
        let mut assistant = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-old", 1);
        assistant.content = vec![Content::Thinking(crate::types::ThinkingContent {
            kind: Default::default(),
            thinking: "visible reasoning".to_string(),
            thinking_signature: Some(
                json!({"type":"reasoning","id":"rs_old","encrypted_content":"opaque"}).to_string(),
            ),
            redacted: false,
        })];
        let body = build_request(
            &model(),
            &Context::new(vec![Message::Assistant(Box::new(assistant))]),
            &SimpleStreamOptions::default(),
        );
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "visible reasoning");
    }

    #[test]
    fn reasoning_summaries_are_requested_on_the_official_endpoint() {
        let mut model = model();
        model.reasoning = true;
        let ctx = Context::new(Vec::new());
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            ..SimpleStreamOptions::default()
        };
        assert!(build_request(&model, &ctx, &opts)["reasoning"]["summary"].is_null());
        model.base_url = "https://api.openai.com/v1".into();
        assert_eq!(
            build_request(&model, &ctx, &opts)["reasoning"]["summary"],
            "auto"
        );
    }

    #[tokio::test]
    async fn reasoning_content_fills_when_summary_is_absent() {
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = ResponsesStreamState::default();
        let mut output = AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1);
        state
            .add_item(
                0,
                &json!({"type":"reasoning","id":"rs_content"}),
                &mut output,
                &mut producer,
            )
            .await;
        state
            .finish_item(
                0,
                &json!({
                    "type":"reasoning",
                    "id":"rs_content",
                    "content":[{"type":"reasoning_text","text":"from content"}]
                }),
                &mut output,
                &mut producer,
            )
            .await;
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::ThinkingStart { .. })
        ));
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::ThinkingDelta { delta, .. }) if delta == "from content"
        ));
        assert!(matches!(
            &output.content[0],
            Content::Thinking(thinking) if thinking.thinking == "from content"
        ));
    }

    #[tokio::test]
    async fn cancelled_terminal_status_is_reported_as_error() {
        let (mut producer, stream) = create_assistant_message_event_stream();
        finish(
            &mut producer,
            AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1),
            &model(),
            Some("cancelled"),
            None,
        )
        .await;
        let result = futures::executor::block_on(stream.result()).unwrap();
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("cancelled")));
    }

    #[tokio::test]
    async fn incomplete_max_output_tokens_maps_to_length() {
        let (mut producer, stream) = create_assistant_message_event_stream();
        finish(
            &mut producer,
            AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1),
            &model(),
            Some("incomplete"),
            Some("max_output_tokens"),
        )
        .await;
        let result = futures::executor::block_on(stream.result()).unwrap();
        assert_eq!(result.stop_reason, StopReason::Length);
        assert_eq!(result.raw_stop_reason.as_deref(), Some("incomplete"));
        assert!(result.error_message.is_none());
    }

    #[tokio::test]
    async fn incomplete_non_token_reason_maps_to_length() {
        let (mut producer, stream) = create_assistant_message_event_stream();
        finish(
            &mut producer,
            AssistantMessage::empty(Api::OpenaiResponses, "openai", "gpt-test", 1),
            &model(),
            Some("incomplete"),
            Some("content_filter"),
        )
        .await;
        let result = futures::executor::block_on(stream.result()).unwrap();
        assert_eq!(result.stop_reason, StopReason::Length);
        assert_eq!(result.raw_stop_reason.as_deref(), Some("incomplete"));
        assert!(result.error_message.is_none());
    }

    #[test]
    fn incomplete_reason_is_read_from_nested_or_top_level_response() {
        let nested = json!({
            "type":"response.incomplete",
            "response":{"incomplete_details":{"reason":"max_output_tokens"}}
        });
        assert_eq!(
            response_incomplete_reason(nested.get("response").unwrap(), &nested,),
            Some("max_output_tokens")
        );

        let top_level = json!({
            "type":"response.incomplete",
            "incomplete_details":{"reason":"content_filter"}
        });
        assert_eq!(
            response_incomplete_reason(&top_level, &top_level),
            Some("content_filter")
        );
    }

    #[tokio::test]
    async fn function_tool_initial_raw_prefix_is_emitted_once_through_real_sse_mapping() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Include a second call with no deltas: its final boundary remains
        // authoritative, without introducing a synthetic delta event.
        let frames = [
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","id":"fc_1","call_id":"call_1","name":"read",
                "arguments":"{\"path\":\"pre"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"fix"}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","id":"fc_1","call_id":"call_1","name":"read",
                "arguments":"{\"path\":\"prefix\"}"}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","id":"fc_2","call_id":"call_2","name":"read",
                "arguments":"{\"path\":\"pre"}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","id":"fc_2","call_id":"call_2","name":"read",
                "arguments":"{\"path\":\"prefix\"}"}}),
            json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
        ];
        let body = frames
            .iter()
            .map(|frame| format!("data: {frame}\r\n\r\n"))
            .collect::<String>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1");
        let provider = OpenAiResponsesProvider::with_models(
            "test",
            Some("test-key".into()),
            reqwest::Client::new(),
            vec![test_model.clone()],
        );
        let mut stream = provider
            .stream_simple(
                &test_model,
                &Context::new(Vec::new()),
                &SimpleStreamOptions::default(),
            )
            .await;
        let mut deltas = Vec::new();
        let mut ends = Vec::new();
        let mut final_message = None;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event {
                    AssistantMessageEvent::ToolCallDelta {
                        content_index,
                        delta,
                        partial,
                    } => {
                        assert_eq!(content_index, 0);
                        let expected = if deltas.is_empty() {
                            "{\"path\":\"prefix"
                        } else {
                            "{\"path\":\"prefix\"}"
                        };
                        assert!(matches!(&partial.content[0], Content::ToolCall(call)
                            if call.arguments == parse_streaming_json(Some(expected))));
                        deltas.push(delta);
                    }
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => ends.push(tool_call),
                    AssistantMessageEvent::Done { message, .. } => final_message = Some(message),
                    AssistantMessageEvent::Error { error, .. } => {
                        panic!("unexpected error: {error:?}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(deltas, ["{\"path\":\"prefix", "\"}"]);
        assert_eq!(
            serde_json::from_str::<Value>(&deltas.concat()).unwrap(),
            json!({"path":"prefix"})
        );
        assert_eq!(ends.len(), 2);
        for call in ends {
            assert_eq!(call.arguments, json!({"path":"prefix"}));
        }
        let final_message = final_message.expect("done");
        assert_eq!(final_message.content.len(), 2);
        for block in &final_message.content {
            assert!(
                matches!(block, Content::ToolCall(call) if call.arguments == json!({"path":"prefix"}))
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_responses_sse_frame_surfaces_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = concat!(
            "event: response.created\r\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\r\n",
            "\r\n",
            "event: response.output_text.delta\r\n",
            "data: {not-json\r\n",
            "\r\n",
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1");
        let provider = OpenAiResponsesProvider::with_models(
            "test",
            Some("test-key".to_string()),
            reqwest::Client::new(),
            vec![test_model.clone()],
        );
        let stream = provider
            .stream_simple(
                &test_model,
                &Context::new(Vec::new()),
                &SimpleStreamOptions::default(),
            )
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("invalid OpenAI Responses SSE event")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn legacy_fc_only_id_is_replayed_in_the_wire_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (body_tx, body_rx) = tokio::sync::oneshot::channel::<Value>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start;
            let content_length;
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending an HTTP request");
                request.extend_from_slice(&chunk[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    body_start = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .expect("reqwest must send Content-Length");
                    break;
                }
            }
            while request.len() < body_start + content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending the full body");
                request.extend_from_slice(&chunk[..read]);
            }
            let payload: Value =
                serde_json::from_slice(&request[body_start..body_start + content_length]).unwrap();
            body_tx.send(payload).unwrap();

            let response_body = concat!(
                "event: response.completed\r\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_legacy\",\"status\":\"completed\",\"output\":[]}}\r\n",
                "\r\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1");
        let provider = OpenAiResponsesProvider::with_models(
            "test",
            Some("test-key".to_string()),
            reqwest::Client::new(),
            vec![test_model.clone()],
        );
        let legacy_id = "fc_Qgl0hBWAvSkQ9YXljCq7WLxS";
        let context = Context::new(vec![
            Message::User(crate::types::UserMessage::new("run the tool", 1)),
            Message::Assistant(Box::new(assistant_with_call(legacy_id))),
            tool_result(legacy_id),
        ]);
        let stream = provider
            .stream_simple(&test_model, &context, &SimpleStreamOptions::default())
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);

        let body = body_rx.await.unwrap();
        let input = body["input"].as_array().unwrap();
        let call_index = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .expect("legacy function call must be replayed");
        let output_index = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .expect("tool result must be serialized");
        assert!(call_index < output_index);
        assert_eq!(input[call_index]["call_id"], legacy_id);
        assert_eq!(input[output_index]["call_id"], legacy_id);

        server.await.unwrap();
    }
}

#[cfg(test)]
mod native_compat_tests {
    use super::*;

    #[test]
    fn supergrok_output_budget_and_reasoning_follow_native_limits() {
        let mut model = Model::new(
            "grok-build-0.1",
            "Grok Build",
            Api::OpenaiResponses,
            "xai-oauth",
            "https://api.x.ai/v1",
        );
        model.reasoning = true;
        model.max_tokens = 1_000_000;
        model.compat = Some(StreamingProtocolCompat::OpenaiResponses(
            crate::model::OpenaiResponsesCompat {
                supports_reasoning_effort: Some(false),
                include_encrypted_reasoning: Some(true),
                ..Default::default()
            },
        ));
        let options = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let request = build_request(&model, &Context::default(), &options);
        assert_eq!(request["max_output_tokens"], 64_000);
        assert!(request.get("reasoning").is_none());
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
    }
}
