//! Mirrors `packages/agent/src/harness/compaction/compaction.ts` — the core
//! compaction pipeline: `prepare_compaction` (cut-point selection + virtual
//! retained-tail re-injection + file-op extraction), `compact` (split-turn TWO
//! LLM calls, invariant §10), `complete_simple_with_retries` (standalone
//! summary request, `CacheRetention::None` + fresh `session_id`), the
//! summarization prompts, `combine_usage`, and the `CompactionError` type.
//!
//! v1 divergences from TS (recorded for review):
//! - **`complete_simple` source.** TS `Models.completeSimple` is a higher-level
//!   helper; pi-ai has only `Provider::stream_simple`. `complete_simple_with_ret
//!   ries` is built over `Provider::stream_simple` + `stream.result()` — the
//!   faithful equivalent, one provider per call. The harness (M5f) resolves
//!   which provider serves the summarization model and passes it in.
//! - **`retry_assistant_call`.** pi-ai has only HTTP-level
//!   `retry_provider_request` (anthropic). This module ports a compaction-local
//!   AssistantMessage-level retry mirroring `retryAssistantCall`'s shape
//!   (abort-terminal, success-fast, exponential backoff, sleep honors the
//!   cancellation token, abort-during-sleep normalized to an aborted message).
//!   The retryability classifier is a conservative *substring* port of the TS
//!   regex table (not full regex), because the workspace has no `regex` dep and
//!   the compaction retry policy defaults to disabled — faux tests never retry,
//!   so the classifier is off the hot path.
//! - **Retry callbacks** (`onRetryScheduled`/`onRetryAttemptStart`/
//!   `onRetryFinished`) are omitted in v1 (telemetry-only in TS); the harness
//!   can add a trait-hook sink later without changing the call shape.
//! - **`build_session_context`** is a minimal port (in `tokens.rs`) pulled
//!   forward from M5f; M5f's Session facade will own/expand it.

use std::sync::Arc;
use std::time::Duration;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{
    AssistantMessage, Content, Context, Message, StopReason, Usage, UserContent, UserMessage,
};
use rpi_ai::{CacheRetention, Model, Provider, SimpleStreamOptions, ThinkingLevel};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::compaction::cut_point::find_cut_point;
use crate::compaction::tokens::{
    build_session_context, compute_file_lists, create_file_ops, estimate_context_tokens,
    extract_file_ops_from_message, format_file_operations, serialize_conversation, FileOperations,
};
use crate::messages::convert_to_llm;
use crate::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::session::types::{Entry, EntryBase, MessageEntry};
use crate::types::{CompactionSettings, RetryPolicy};

// ---------------------------------------------------------------------------
// Prompts — verbatim from compaction.ts
// ---------------------------------------------------------------------------

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `CompactionError` code. Mirrors TS `"aborted" | "summarization_failed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionErrorCode {
    Aborted,
    SummarizationFailed,
}

impl CompactionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionErrorCode::Aborted => "aborted",
            CompactionErrorCode::SummarizationFailed => "summarization_failed",
        }
    }
}

/// Mirrors TS `CompactionError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionError {
    pub code: CompactionErrorCode,
    pub message: String,
}

impl CompactionError {
    pub fn aborted(message: impl Into<String>) -> Self {
        Self {
            code: CompactionErrorCode::Aborted,
            message: message.into(),
        }
    }
    pub fn summarization_failed(message: impl Into<String>) -> Self {
        Self {
            code: CompactionErrorCode::SummarizationFailed,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for CompactionError {}

// ---------------------------------------------------------------------------
// Results / preparation
// ---------------------------------------------------------------------------

/// File-operation details stored on generated compaction entries. Mirrors TS
/// `CompactionDetails`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Generated compaction data ready to be persisted as a compaction entry.
/// Mirrors TS `CompactResult<T>`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactResult {
    pub summary: String,
    pub tokens_before: i64,
    pub usage: Option<Usage>,
    pub retained_tail: Vec<AgentMessage>,
    pub details: Option<CompactionDetails>,
}

/// Prepared inputs for a compaction run. Mirrors TS `CompactionPreparation`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPreparation {
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub retained_tail: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: i64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// Provider/model/etc. needed for the standalone summarization LLM call(s).
/// Consolidates the `models, model, signal, customInstructions, thinkingLevel,
/// retry` TS args into one struct so `compact`/`generate_*` stay readable.
#[derive(Clone)]
pub struct CompactionLlmOptions {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub api_key: Option<String>,
    pub signal: CancellationToken,
    pub thinking_level: Option<ThinkingLevel>,
    pub retry: Option<RetryPolicy>,
    pub custom_instructions: Option<String>,
    /// Pinned request controls, including total and stream idle timeouts.
    pub stream_options: crate::types::AgentHarnessStreamOptions,
}

// ---------------------------------------------------------------------------
// Entry → message helpers — mirrors getMessageFromEntry{,ForCompaction}
// ---------------------------------------------------------------------------

/// `getMessageFromEntry`. `message` → its message; `branch_summary` → a
/// branch-summary message; `compaction` → a compaction-summary message; else
/// `None`.
pub fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message(m) => Some(m.message.clone()),
        Entry::BranchSummary(b) => Some(create_branch_summary_message(
            &b.summary,
            &b.from_id,
            b.base.timestamp,
        )),
        Entry::Compaction(c) => Some(create_compaction_summary_message(
            &c.summary,
            c.tokens_before,
            c.base.timestamp,
        )),
        _ => None,
    }
}

/// `getMessageFromEntryForCompaction` — compaction entries are not summarized
/// (they ARE the summary), so skip them.
pub fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    if matches!(entry, Entry::Compaction(_)) {
        return None;
    }
    get_message_from_entry(entry)
}

/// The timestamp carried by an `AgentMessage` (each variant stores one).
fn agent_message_timestamp(message: &AgentMessage) -> i64 {
    match message {
        AgentMessage::User(u) => u.timestamp,
        AgentMessage::Assistant(a) => a.timestamp,
        AgentMessage::ToolResult(t) => t.timestamp,
        AgentMessage::Custom(c) => c.timestamp,
    }
}

// ---------------------------------------------------------------------------
// extractFileOperations — seeds from prev compaction details + summarized msgs
// ---------------------------------------------------------------------------

/// `extractFileOperations`. Seeds `read`/`edited` from a previous compaction's
/// `details` (so file lists accumulate across compactions), then extracts from
/// each summarized assistant message.
pub fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[Entry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut ops = create_file_ops();
    if let Some(i) = prev_compaction_index {
        if let Some(Entry::Compaction(c)) = entries.get(i) {
            if let Some(details) = &c.details {
                if let Some(arr) = details.get("readFiles").and_then(|v| v.as_array()) {
                    for f in arr {
                        if let Some(s) = f.as_str() {
                            ops.read.insert(s.to_string());
                        }
                    }
                }
                if let Some(arr) = details.get("modifiedFiles").and_then(|v| v.as_array()) {
                    for f in arr {
                        if let Some(s) = f.as_str() {
                            ops.edited.insert(s.to_string());
                        }
                    }
                }
            }
        }
    }
    for msg in messages {
        extract_file_ops_from_message(msg, &mut ops);
    }
    ops
}

// ---------------------------------------------------------------------------
// prepareCompaction
// ---------------------------------------------------------------------------

/// Prepare session entries for compaction, or `None` when compaction is not
/// applicable (empty path, or last entry is already a compaction). Mirrors
/// `prepareCompaction`.
///
/// The returned `Result` is always `Ok` (TS never errors here), but carries the
/// `CompactionError` type for parity with the TS signature.
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty() {
        return Ok(None);
    }
    if matches!(path_entries.last(), Some(Entry::Compaction(_))) {
        return Ok(None);
    }

    let prev_compaction_index = path_entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, e)| matches!(e, Entry::Compaction(_)))
        .map(|(i, _)| i);

    let (previous_summary, compactable_entries): (Option<String>, Vec<Entry>) =
        match prev_compaction_index {
            Some(i) => {
                let prev = match &path_entries[i] {
                    Entry::Compaction(c) => c.clone(),
                    _ => unreachable!("prev_compaction_index points at a compaction"),
                };
                let prev_summary = prev.summary.clone();
                let prev_id = prev.base.id.clone();
                let prev_seq = prev.base.seq;
                let mut virtual_entries: Vec<Entry> = Vec::with_capacity(prev.retained_tail.len());
                for (idx, message) in prev.retained_tail.iter().enumerate() {
                    let id = format!("{prev_id}:retained:{idx}");
                    let parent_id = if idx == 0 {
                        Some(prev_id.clone())
                    } else {
                        Some(format!("{prev_id}:retained:{}", idx - 1))
                    };
                    let timestamp = agent_message_timestamp(message);
                    virtual_entries.push(Entry::Message(MessageEntry {
                        base: EntryBase {
                            entry_type: "message".into(),
                            id,
                            seq: prev_seq,
                            parent_id,
                            timestamp,
                        },
                        message: message.clone(),
                        terminate: None,
                    }));
                }
                // `…virtualRetainedEntries, ...pathEntries.slice(prevCompactionIndex + 1)`.
                let rest: Vec<Entry> = path_entries[i + 1..].to_vec();
                virtual_entries.extend(rest);
                (Some(prev_summary), virtual_entries)
            }
            None => (None, path_entries.to_vec()),
        };

    let boundary_end = compactable_entries.len();

    // `tokensBefore` uses the ORIGINAL path entries (not compactable), matching
    // TS `buildSessionContext(pathEntries).messages`.
    let tokens_before =
        estimate_context_tokens(&build_session_context(path_entries).messages).tokens;

    let cut_point = find_cut_point(
        &compactable_entries,
        0,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut_point.is_split_turn {
        cut_point
            .turn_start_index
            .expect("is_split_turn ⇒ turn_start_index is Some")
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
    for entry in compactable_entries.iter().take(history_end) {
        if let Some(m) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(m);
        }
    }

    let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
    if cut_point.is_split_turn {
        let turn_start = cut_point
            .turn_start_index
            .expect("is_split_turn ⇒ turn_start_index is Some");
        for entry in compactable_entries
            .iter()
            .take(cut_point.first_kept_entry_index)
            .skip(turn_start)
        {
            if let Some(m) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(m);
            }
        }
    }

    let mut retained_tail: Vec<AgentMessage> = Vec::new();
    for entry in compactable_entries
        .iter()
        .skip(cut_point.first_kept_entry_index)
    {
        if let Some(m) = get_message_from_entry_for_compaction(entry) {
            retained_tail.push(m);
        }
    }

    let mut file_ops =
        extract_file_operations(&messages_to_summarize, path_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    }))
}

// ---------------------------------------------------------------------------
// completeSimpleWithRetries + retryAssistantCall
// ---------------------------------------------------------------------------

/// `completeSimpleWithRetries`. Summaries are standalone requests: force
/// `CacheRetention::None` and a fresh `session_id` (so a summary never writes a
/// reusable cache entry), then run the call through AssistantMessage-level
/// retry.
pub async fn complete_simple_with_retries(
    provider: Arc<dyn Provider>,
    model: Model,
    context: Context,
    mut options: SimpleStreamOptions,
    retry: Option<RetryPolicy>,
) -> AssistantMessage {
    // Isolate routing + avoid cache writes that cannot be reused.
    options.cache_retention = CacheRetention::None;
    options.session_id = Some(Uuid::now_v7().to_string());
    let signal = options.signal.clone();
    retry_assistant_call(
        move || {
            let provider = Arc::clone(&provider);
            let model = model.clone();
            let context = context.clone();
            let options = options.clone();
            async move {
                let stream = provider.stream_simple(&model, &context, &options).await;
                match stream.result().await {
                    Ok(msg) => msg,
                    Err(_) => AssistantMessage::terminal(
                        model.api.clone(),
                        provider.id().to_string(),
                        model.id.clone(),
                        StopReason::Error,
                        "Summarization stream ended without a terminal event",
                        0,
                    ),
                }
            }
        },
        retry,
        &signal,
    )
    .await
}

/// `combineUsage(first, second)`. `Usage::add` already sums every field
/// (including the optional `cache_write_1h`/`reasoning` with None-as-0
/// semantics + costs); this is the functional wrapper the TS code uses.
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let mut u = first.clone();
    u.add(second);
    u
}

/// Run a single assistant-producing call with bounded retry on transient
/// errors. Mirrors `retryAssistantCall`: aborts are terminal (never retried);
/// success returns; non-retryable or exhausted returns the final error;
/// otherwise exponential backoff `base_delay_ms * 2^(attempt-1)`, with the
/// sleep honoring `signal` — an abort during sleep is normalized to an aborted
/// `AssistantMessage` (matching TS's `stopReason:"aborted"` normalization).
async fn retry_assistant_call<F, Fut>(
    produce: F,
    retry: Option<RetryPolicy>,
    signal: &CancellationToken,
) -> AssistantMessage
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = AssistantMessage>,
{
    let policy = retry.filter(|p| p.enabled);
    let max_attempts: u32 = policy.as_ref().map(|p| p.max_retries).unwrap_or(0);
    let base_delay_ms: u64 = policy.as_ref().map(|p| p.base_delay_ms).unwrap_or(1000);
    let max_agent_delay_ms: u64 = policy
        .as_ref()
        .map(|p| p.max_agent_delay_ms)
        .unwrap_or(60_000);

    let mut attempt: u32 = 0;
    loop {
        let response = produce().await;

        // Abort: terminal but not successful; never retry.
        if response.stop_reason == StopReason::Aborted {
            return response;
        }
        // Success: non-error, non-abort.
        if response.stop_reason != StopReason::Error {
            return response;
        }
        // Non-retryable or budget exhausted: return the final error.
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            return response;
        }
        attempt += 1;
        let delay_ms = base_delay_ms
            .saturating_mul(1u64 << (attempt - 1))
            .min(max_agent_delay_ms);
        // Sleep honoring cancellation → normalize to aborted.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            _ = signal.cancelled() => {
                let mut aborted = response;
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = None;
                return aborted;
            }
        }
    }
}

/// Conservative substring port of `isRetryableAssistantError`. Returns false
/// for quota/billing errors (non-retryable) and true only for clearly-transient
/// transport/server text. See module doc for the divergence note.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(msg) = &message.error_message else {
        return false;
    };
    let lower = msg.to_lowercase();
    const NON_RETRYABLE: &[&str] = &[
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ];
    if NON_RETRYABLE.iter().any(|p| lower.contains(p)) {
        return false;
    }
    const RETRYABLE: &[&str] = &[
        "overloaded",
        "rate limit",
        "too many requests",
        "429",
        "500",
        "502",
        "503",
        "504",
        "524",
        "service unavailable",
        "server error",
        "internal error",
        "provider returned error",
        "exceeded request buffer limit while retrying upstream",
        "network error",
        "connection error",
        "connection refused",
        "connection lost",
        "other side closed",
        "fetch failed",
        "getaddrinfo",
        "enotfound",
        "eai_again",
        "upstream connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed out",
        "timeout",
        "websocket closed",
        "websocket error",
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "retry delay",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "resourceexhausted",
    ];
    RETRYABLE.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// generateSummaryWithUsage / generateTurnPrefixSummary
// ---------------------------------------------------------------------------

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `floor(0.8 * reserveTokens)`, capped by `model.max_tokens` when > 0.
fn summary_max_tokens(reserve_tokens: i64, model_max_tokens: u64) -> u64 {
    let reserve = reserve_tokens.max(0) as u64;
    let budget = ((reserve as f64) * 0.8).floor() as u64;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

/// `floor(0.5 * reserveTokens)`, capped by `model.max_tokens` when > 0.
fn turn_prefix_max_tokens(reserve_tokens: i64, model_max_tokens: u64) -> u64 {
    let reserve = reserve_tokens.max(0) as u64;
    let budget = ((reserve as f64) * 0.5).floor() as u64;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

/// Build the per-call `SimpleStreamOptions` from the LLM options + a resolved
/// `max_tokens`. Sets `api_key`/`signal`/`max_tokens`/`reasoning` (reasoning
/// only when the model reasons AND a non-`Off` level is set — mirrors TS).
fn build_summary_options(opts: &CompactionLlmOptions, max_tokens: u64) -> SimpleStreamOptions {
    let mut o = opts
        .stream_options
        .merge_into(SimpleStreamOptions::default());
    o.api_key = opts.api_key.clone();
    o.signal = opts.signal.clone();
    o.max_tokens = Some(max_tokens);
    if opts.model.reasoning {
        if let Some(tl) = opts.thinking_level {
            if tl != ThinkingLevel::Off {
                o.reasoning = Some(tl);
            }
        }
    }
    o
}

/// `generateSummaryWithUsage`. Builds the conversation+prompt user message,
/// calls `complete_simple_with_retries`, maps `aborted`/`error` stop reasons to
/// `CompactionError`. Returns `(text, usage)`.
pub async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    reserve_tokens: i64,
    previous_summary: Option<&str>,
    opts: &CompactionLlmOptions,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = summary_max_tokens(reserve_tokens, opts.model.max_tokens);

    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    }
    .to_string();
    if let Some(ci) = &opts.custom_instructions {
        base_prompt += &format!("\n\nAdditional focus: {ci}");
    }

    let llm_messages = convert_to_llm(current_messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(prev) = previous_summary {
        prompt_text += &format!("<previous-summary>\n{prev}\n</previous-summary>\n\n");
    }
    prompt_text += &base_prompt;

    let summarization_messages = vec![Message::User(UserMessage::new(
        UserContent::Blocks(vec![Content::text(prompt_text)]),
        now_ms(),
    ))];
    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: Vec::new(),
    };
    let options = build_summary_options(opts, max_tokens);

    let response = complete_simple_with_retries(
        opts.provider.clone(),
        opts.model.clone(),
        context,
        options,
        opts.retry.clone(),
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::aborted(
            response
                .error_message
                .as_deref()
                .unwrap_or("Summarization aborted"),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::summarization_failed(format!(
            "Summarization failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        )));
    }

    let text = Content::text_only(&response.content, "\n");
    Ok((text, response.usage.clone()))
}

/// `generateTurnPrefixSummary`. Same shape as the history summary but with the
/// turn-prefix prompt, `0.5×reserve` max-tokens, no previous-summary, no custom
/// instructions.
pub async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    reserve_tokens: i64,
    opts: &CompactionLlmOptions,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = turn_prefix_max_tokens(reserve_tokens, opts.model.max_tokens);

    let llm_messages = convert_to_llm(messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let summarization_messages = vec![Message::User(UserMessage::new(
        UserContent::Blocks(vec![Content::text(prompt_text)]),
        now_ms(),
    ))];
    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: Vec::new(),
    };
    let options = build_summary_options(opts, max_tokens);

    let response = complete_simple_with_retries(
        opts.provider.clone(),
        opts.model.clone(),
        context,
        options,
        opts.retry.clone(),
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::aborted(
            response
                .error_message
                .as_deref()
                .unwrap_or("Turn prefix summarization aborted"),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::summarization_failed(format!(
            "Turn prefix summarization failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        )));
    }

    let text = Content::text_only(&response.content, "\n");
    Ok((text, response.usage.clone()))
}

// ---------------------------------------------------------------------------
// compact — split-turn TWO LLM calls (invariant §10)
// ---------------------------------------------------------------------------

/// `compact`. Generates the compaction summary from a preparation. Split-turn
/// with a non-empty turn prefix → TWO LLM calls (history @ 0.8×reserve, turn
/// prefix @ 0.5×reserve) concatenated as `"{history}\n\n---\n\n**Turn Context
/// (split turn):**\n\n{turnPrefix}"`; otherwise a single history call. Appends
/// `format_file_operations`. Returns `CompactResult`.
pub async fn compact(
    preparation: &CompactionPreparation,
    opts: &CompactionLlmOptions,
) -> Result<CompactResult, CompactionError> {
    let (summary, usage) =
        if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
            let (history_text, history_usage) = if preparation.messages_to_summarize.is_empty() {
                ("No prior history.".to_string(), None)
            } else {
                let (t, u) = generate_summary_with_usage(
                    &preparation.messages_to_summarize,
                    preparation.settings.reserve_tokens,
                    preparation.previous_summary.as_deref(),
                    opts,
                )
                .await?;
                (t, Some(u))
            };
            let (turn_text, turn_usage) = generate_turn_prefix_summary(
                &preparation.turn_prefix_messages,
                preparation.settings.reserve_tokens,
                opts,
            )
            .await?;
            let combined =
                format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_text}");
            let combined_usage = match history_usage {
                Some(hu) => Some(combine_usage(&hu, &turn_usage)),
                None => Some(turn_usage),
            };
            (combined, combined_usage)
        } else {
            let (t, u) = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                preparation.settings.reserve_tokens,
                preparation.previous_summary.as_deref(),
                opts,
            )
            .await?;
            (t, Some(u))
        };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let file_ops_text = format_file_operations(&read_files, &modified_files);
    let summary = format!("{summary}{file_ops_text}");

    Ok(CompactResult {
        summary,
        tokens_before: preparation.tokens_before,
        usage,
        retained_tail: preparation.retained_tail.clone(),
        details: Some(CompactionDetails {
            read_files,
            modified_files,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::EntryBase;

    #[test]
    fn compaction_inherits_total_and_idle_timeouts() {
        let provider =
            rpi_ai::providers::faux::FauxProvider::new(rpi_ai::providers::faux::FauxScript::new());
        let opts = CompactionLlmOptions {
            model: provider.default_model().clone(),
            provider,
            api_key: None,
            signal: CancellationToken::new(),
            thinking_level: None,
            retry: None,
            custom_instructions: None,
            stream_options: crate::types::AgentHarnessStreamOptions {
                timeout: Some(Duration::from_secs(30)),
                stream_idle_timeout: Some(Duration::ZERO),
                ..Default::default()
            },
        };
        let options = build_summary_options(&opts, 42);
        assert_eq!(options.request_timeout(), Duration::from_secs(30));
        assert_eq!(options.stream_idle_timeout(), None);
        assert_eq!(options.max_tokens, Some(42));
    }

    fn user_msg(text: &str, seq: u64) -> Entry {
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: format!("e{seq}"),
                seq,
                parent_id: None,
                timestamp: seq as i64,
            },
            message: AgentMessage::User(UserMessage::new(
                UserContent::Text(text.into()),
                seq as i64,
            )),
            terminate: None,
        })
    }

    #[test]
    fn prepare_compaction_empty_is_none() {
        let r = prepare_compaction(&[], CompactionSettings::default()).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn prepare_compaction_last_is_compaction_is_none() {
        let entries = vec![user_msg("hi", 1)];
        let compaction = Entry::Compaction(crate::session::types::CompactionEntry {
            base: EntryBase {
                entry_type: "compaction".into(),
                id: "c1".into(),
                seq: 2,
                parent_id: Some("e1".into()),
                timestamp: 2,
            },
            summary: "s".into(),
            retained_tail: Vec::new(),
            tokens_before: 10,
            details: None,
            usage: None,
        });
        let r = prepare_compaction(
            &[entries[0].clone(), compaction],
            CompactionSettings::default(),
        )
        .unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn prepare_compaction_single_user_returns_some_with_empty_summarize() {
        // One user message, small keep_recent — no history to summarize.
        let entries = vec![user_msg("hi", 1)];
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 10,
        };
        let p = prepare_compaction(&entries, settings).unwrap().unwrap();
        assert!(p.messages_to_summarize.is_empty());
        assert!(p
            .retained_tail
            .iter()
            .any(|m| matches!(m, AgentMessage::User(u) if u.content.as_text()==Some("hi"))));
        assert!(!p.is_split_turn);
    }

    #[test]
    fn combine_usage_sums_fields() {
        let a = Usage {
            input: 10,
            output: 5,
            cache_read: 1,
            cache_write: 2,
            cache_write_1h: Some(3),
            reasoning: None,
            total_tokens: 18,
            cost: rpi_ai::types::UsageCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 0.25,
                total: 3.75,
            },
        };
        let b = Usage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: Some(4),
            total_tokens: 2,
            cost: rpi_ai::types::UsageCost {
                input: 0.1,
                output: 0.2,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.3,
            },
        };
        let c = combine_usage(&a, &b);
        assert_eq!(c.input, 11);
        assert_eq!(c.output, 6);
        assert_eq!(c.cache_write_1h, Some(3));
        assert_eq!(c.reasoning, Some(4));
        assert_eq!(c.total_tokens, 20);
        assert!((c.cost.total - 4.05).abs() < 1e-9);
    }

    #[test]
    fn extract_file_operations_seeds_from_prev_compaction_details() {
        let details = serde_json::json!({"readFiles": ["/r"], "modifiedFiles": ["/m"]});
        let compaction = Entry::Compaction(crate::session::types::CompactionEntry {
            base: EntryBase {
                entry_type: "compaction".into(),
                id: "c1".into(),
                seq: 1,
                parent_id: None,
                timestamp: 1,
            },
            summary: "s".into(),
            retained_tail: Vec::new(),
            tokens_before: 0,
            details: Some(details),
            usage: None,
        });
        let ops = extract_file_operations(&[], &[compaction], Some(0));
        assert!(ops.read.contains("/r"));
        assert!(ops.edited.contains("/m"));
    }

    /// A host error that fails a turn closed so a recorded tool call cannot run
    /// must never be retried automatically: a retry asks a remote agent to run
    /// the turn again. A transient-looking error stays retryable.
    #[test]
    fn recorded_call_fail_closed_errors_are_not_retryable() {
        let error = |text: &str| {
            let mut message = AssistantMessage::empty(rpi_ai::types::Api::Faux, "p", "m", 0);
            message.stop_reason = StopReason::Error;
            message.error_message = Some(text.to_string());
            message
        };
        // These mirror the fixed fail-closed texts in rpi-extensions (provider.rs)
        // and rpi-plugin-sdk (provider.rs), which have exact-text tests there.
        let texts = [
            "a recorded call has no id",
            "two recorded calls share an id",
            "the call is missing",
            "the call is repeated",
            "its name or arguments changed",
            "its recording differs",
        ]
        .map(|reason| {
            format!("plugin provider final message drops a recorded tool call: {reason}")
        });
        let sdk = "host does not support recorded tool calls".to_string();
        for text in texts.iter().chain([&sdk]) {
            assert!(!is_retryable_assistant_error(&error(text)), "{text}");
        }
        assert!(is_retryable_assistant_error(&error(
            "503 service unavailable"
        )));
    }
}
