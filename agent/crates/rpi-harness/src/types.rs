//! Mirrors `packages/agent/src/harness/types.ts` (harness-level surface only).
//!
//! The TS `types.ts` is split: a `FileSystem`/`Shell`/`ExecutionEnv` section
//! (ported in M4 to `pi-tools/src/env.rs`) and a harness-configuration section
//! (`Skill`, `PromptTemplate`, `AgentHarnessResources`, `AgentHarnessTool`,
//! `AgentHarnessStreamOptions` + `Patch`, `AgentHarnessOptions`, `DrivingMode`).
//! This module mirrors the latter; it re-exports the M4 env types via `rpi_tools`
//! where the harness needs them.
//!
//! `Result`/`ok`/`err`/`getOrThrow`/`toError` from the same TS file are dropped —
//! Rust has `Result` natively and `?`/`.map_err` replace the helpers.

use std::collections::BTreeMap;
use std::sync::Arc;

use rpi_agent::{
    AfterToolCall, AgentTool, BeforeToolCall, ConvertToLlm, QueueMode, TransformContext,
};
use rpi_ai::{CacheRetention, Model, Provider, ProviderHooks, SimpleStreamOptions, ThinkingLevel};
use serde::{Deserialize, Serialize};

use crate::session::context::{ContextEntryTransform, CustomEntryContextMessageProjector};
use crate::session::Session;

/// When a message sent during a run becomes model input. Matches Pi's
/// `deliverAs` values; `NextTurn` waits for a new explicit prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageDelivery {
    #[default]
    Steer,
    FollowUp,
    NextTurn,
}

/// Pi-compatible message delivery options. Omission of `triggerTurn` appends
/// context while idle and steers while running. Explicit `false` never starts
/// another model turn. `nextTurn` takes precedence over `triggerTurn`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_turn: Option<bool>,
    pub deliver_as: MessageDelivery,
}

/// A skill loaded from a `SKILL.md` file or provided by an application. Mirrors
/// TS `Skill`.
///
/// `name`/`description`/`file_path` are inserted into the system prompt in an
/// XML-formatted block (see `skills.rs::format_skills_for_system_prompt`).
/// `disable_model_invocation` excludes the skill from model-visible listings
/// while still allowing explicit application invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    /// Absolute path to the skill file. Used for model-visible location and
    /// resolving relative references in invocations.
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_model_invocation: Option<bool>,
}

/// A prompt template that can be formatted into a prompt for explicit
/// invocation. Mirrors TS `PromptTemplate`. Argument placeholders are
/// substituted by `prompt_templates.rs::substitute_args`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content: String,
}

/// Resources made available to explicit invocation methods and system-prompt
/// callbacks. Mirrors TS `AgentHarnessResources`. Fields clone-on-get per
/// the defensive-copy contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentHarnessResources {
    /// Paths of files successfully used to compose the system prompt, in load
    /// order: base prompt, append prompts, then project instruction files.
    /// This is provenance for resource listings; contents are already composed
    /// into the system prompt and must not be injected again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_templates: Option<Vec<PromptTemplate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Skill>>,
}

impl AgentHarnessResources {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// A tool definition executed by the harness with an application-defined
/// context. Mirrors TS `AgentHarnessTool` = `Omit<AgentTool, "execute"> & {
/// execute(..., context) }`.
///
/// The Rust port stays an `AgentTool` at the schema level (the harness stores
/// `Arc<dyn AgentTool>` for the loop) and layers a separate
/// `AgentHarnessToolContextSource` + plumbing in `agent_harness.rs` that injects
/// the per-turn context before delegating `execute`. This type captures the
/// harness-specific *replay* flag the TS union adds (`replay?: "never"|"safe"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolReplay {
    /// The tool must not be re-run when a crash left its outcome unknown.
    ///
    /// This is the default, matching upstream (`tool.replay ?? "never"`,
    /// `drive/tools.ts`). Defaulting the other way is a safety bug: the first
    /// time recovery acts on this flag, every tool that never opted in —
    /// `bash`, `write`, `edit` — would be replayed.
    #[default]
    Never,
    /// Re-running the call is harmless (read-only or idempotent), so recovery
    /// may re-execute it instead of reporting an unknown outcome. Only tools
    /// that are genuinely free of side effects should declare this.
    Safe,
}

/// A harness tool = an `AgentTool` plus a replay policy. The harness registry
/// stores `HarnessTool` and projects the `AgentTool` to the loop.
#[derive(Clone)]
pub struct HarnessTool {
    pub tool: Arc<dyn AgentTool>,
    pub replay: ToolReplay,
}

impl HarnessTool {
    pub fn new(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            replay: ToolReplay::default(),
        }
    }
    pub fn with_replay(mut self, replay: ToolReplay) -> Self {
        self.replay = replay;
        self
    }
}

/// Convert a `HarnessTool` replay policy to the session record variant.
impl From<ToolReplay> for crate::session::types::ToolReplay {
    fn from(r: ToolReplay) -> Self {
        match r {
            ToolReplay::Never => crate::session::types::ToolReplay::Never,
            ToolReplay::Safe => crate::session::types::ToolReplay::Safe,
        }
    }
}

/// Static tool context or zero-argument provider resolved for each turn
/// snapshot. Mirrors TS `AgentHarnessToolContextSource`.
pub enum AgentHarnessToolContextSource<T: Send + Sync + 'static> {
    Static(T),
    Provider(Arc<dyn Fn() -> futures::future::BoxFuture<'static, T> + Send + Sync>),
}

/// Curated provider request options owned by the harness and snapshotted per
/// turn. Mirrors TS `AgentHarnessStreamOptions`. Subset of `SimpleStreamOptions`
/// the harness lets the caller pin; the harness fills the rest (api key, signal,
/// reasoning, thinking_budgets, session_id) per turn.
#[derive(Debug, Clone, Default)]
pub struct AgentHarnessStreamOptions {
    pub timeout: Option<std::time::Duration>,
    /// Maximum silence between streamed body bytes; zero disables it.
    pub stream_idle_timeout: Option<std::time::Duration>,
    pub max_retries: Option<u32>,
    pub max_retry_delay: Option<std::time::Duration>,
    pub headers: Option<BTreeMap<String, String>>,
    pub metadata: Option<BTreeMap<String, String>>,
    pub cache_retention: Option<CacheRetention>,
    /// Budgets for budget-based thinking (settings `thinkingBudgets`).
    pub thinking_budgets: Option<rpi_ai::ThinkingBudgets>,
}

/// The harness's pinned subset folded into a real [`SimpleStreamOptions`] per
/// turn. `Option<CacheRetention>` defaults when unset so the caller only pins
/// the fields they care about.
impl AgentHarnessStreamOptions {
    /// Merge these pinned options into a base `SimpleStreamOptions`, with this
    /// set taking precedence on `Some` fields.
    pub fn merge_into(&self, mut base: SimpleStreamOptions) -> SimpleStreamOptions {
        if self.timeout.is_some() {
            base.timeout = self.timeout;
        }
        if self.stream_idle_timeout.is_some() {
            base.stream_idle_timeout = self.stream_idle_timeout;
        }
        if self.max_retries.is_some() {
            base.max_retries = self.max_retries;
        }
        if self.max_retry_delay.is_some() {
            base.max_retry_delay = self.max_retry_delay;
        }
        // headers / metadata merge (overlay keys), per the TS Patch semantics.
        if let Some(h) = &self.headers {
            let mut merged = base.headers.unwrap_or_default();
            for (k, v) in h {
                rpi_ai::provider::set_http_header(&mut merged, k, Some(v));
            }
            base.headers = Some(merged);
        }
        if let Some(m) = &self.metadata {
            let mut merged = base.metadata.unwrap_or_default();
            for (k, v) in m {
                merged.insert(k.clone(), v.clone());
            }
            base.metadata = Some(merged);
        }
        if let Some(c) = self.cache_retention {
            base.cache_retention = c;
        }
        if self.thinking_budgets.is_some() {
            base.thinking_budgets = self.thinking_budgets;
        }
        base
    }
}

/// Per-request stream-option patch returned by provider hooks. Mirrors TS
/// `AgentHarnessStreamOptionsPatch` — outer `None` leaves a field unchanged;
/// map entries merge, and inner `None` deletes that entry.
#[derive(Debug, Clone, Default)]
pub struct AgentHarnessStreamOptionsPatch {
    pub timeout: Option<Option<std::time::Duration>>,
    /// Pin or unpin the stream idle timeout; an explicit zero disables it.
    pub stream_idle_timeout: Option<Option<std::time::Duration>>,
    pub max_retries: Option<Option<u32>>,
    pub max_retry_delay: Option<Option<std::time::Duration>>,
    /// `None` = leave; `Some(map)` = merge, with inner `None` deleting keys.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub metadata: Option<BTreeMap<String, Option<serde_json::Value>>>,
    pub cache_retention: Option<Option<CacheRetention>>,
}

impl AgentHarnessStreamOptionsPatch {
    /// Apply this patch to `opts` in place. Mirrors the TS apply semantics:
    /// `undefined` value deletes a key, explicit empty patch clears nothing.
    pub fn apply(&self, opts: &mut AgentHarnessStreamOptions) {
        if let Some(v) = self.timeout {
            opts.timeout = v;
        }
        if let Some(v) = self.stream_idle_timeout {
            opts.stream_idle_timeout = v;
        }
        if let Some(v) = self.max_retries {
            opts.max_retries = v;
        }
        if let Some(v) = self.max_retry_delay {
            opts.max_retry_delay = v;
        }
        if let Some(patch) = &self.headers {
            let mut map = opts.headers.take().unwrap_or_default();
            for (k, v) in patch {
                rpi_ai::provider::set_http_header(&mut map, k, v.as_deref());
            }
            opts.headers = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(patch) = &self.metadata {
            let mut map = opts.metadata.take().unwrap_or_default();
            for (k, v) in patch {
                match v {
                    Some(val) => {
                        map.insert(k.clone(), val.to_string());
                    }
                    None => {
                        map.remove(k);
                    }
                }
            }
            opts.metadata = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(v) = self.cache_retention {
            opts.cache_retention = v;
        }
    }
}

/// Retry policy owned by the harness. Mirrors TS `RetryPolicy` (a thin config
/// the harness folds into `SimpleStreamOptions.max_retries`/`max_retry_delay`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    /// Exponential delay base for assistant-level retries.
    pub base_delay_ms: u64,
    /// Hard cap for the accumulated agent retry delay. Mirrors pi's
    /// `maxAgentDelayMs` (60 seconds by default).
    pub max_agent_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // Pi retries transient assistant failures by default. Provider
            // SDK retries remain disabled separately in the agent stream
            // options so one failure is not retried twice.
            enabled: true,
            max_retries: 10,
            base_delay_ms: 2000,
            max_agent_delay_ms: 60_000,
        }
    }
}

/// Compaction thresholds + retention. Mirrors TS `CompactionSettings`. Defaults
/// `{ enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000 }` — the
/// canonical [`DEFAULT_COMPACTION_SETTINGS`] constant lives here so it is
/// available before the `compaction` module (M5d) is built; that module
/// re-exports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
}

/// Default compaction settings used by the harness. Mirrors TS
/// `DEFAULT_COMPACTION_SETTINGS`.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

/// How the harness drives the loop. Mirrors TS `drive` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrivingMode {
    /// Harness drives the loop to completion automatically.
    #[default]
    Automatic,
    /// Caller peeks/executes actions one at a time (`peek_action`/`execute_action`).
    Manual,
}

/// How a batch of tool calls executes. Mirrors TS `toolExecution` option; maps
/// 1:1 to `rpi_agent::ToolExecutionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessToolExecution {
    #[default]
    Parallel,
    Sequential,
}

impl HarnessToolExecution {
    pub fn to_agent_mode(self) -> rpi_agent::ToolExecutionMode {
        match self {
            HarnessToolExecution::Parallel => rpi_agent::ToolExecutionMode::Parallel,
            HarnessToolExecution::Sequential => rpi_agent::ToolExecutionMode::Sequential,
        }
    }
}

/// Re-export of `rpi_tools::ExecutionEnv` so harness consumers can reach the env
/// trait through `rpi_harness::types::ExecutionEnv` (mirroring how `types.ts`
/// declares `ExecutionEnv` itself).
pub use rpi_tools::ExecutionEnv;

/// Re-export the operation-kind enum so callers referencing it via the harness
/// root see the TS-equivalent name.
pub use crate::result::OperationKind as HarnessOperationKind;

/// `Usage` re-export for harness-record helpers.
pub use rpi_ai::types::Usage as HarnessUsage;

/// Reserved container for the `AgentHarnessOptions` config. Populated in M5f
/// when the `AgentHarness` is implemented; declared now so downstream modules
/// can reference the shape. Mirrors TS `AgentHarnessOptions`.
pub struct AgentHarnessOptions {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub active_tool_names: Vec<String>,
    pub tools: Vec<HarnessTool>,
    pub system_prompt: Option<String>,
    pub resources: AgentHarnessResources,
    pub stream_options: AgentHarnessStreamOptions,
    pub retry: RetryPolicy,
    pub compaction: CompactionSettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: HarnessToolExecution,
    pub drive: DrivingMode,

    // ---- M5f additions (the pieces the real run loop needs) ---------------
    /// The durable session the harness drives. Required: every run persists
    /// entries/records through this facade (mirrors TS `options.session`).
    pub session: Session,

    /// Allow [`AgentHarness::create`] to proceed when the session already has
    /// records. `false` (default) keeps the v1 guard that rejects a non-empty
    /// session (`create.restore is not implemented`); the `--continue`/
    /// `--resume`/`--session` restore path sets it so the harness loads the
    /// existing transcript and keeps appending to the same file.
    pub allow_existing_session: bool,

    /// Provider registry keyed by `model.provider`. The harness resolves which
    /// provider serves the configured `model` (and any compaction model) and
    /// calls `provider.stream_simple` inside the `StreamFn` it builds. Mirrors
    /// TS `options.models`.
    pub models: Vec<Arc<dyn Provider>>,

    /// The agent-level `convert_to_llm` (AgentMessage[] → Message[]). The
    /// harness installs the harness-level converter (which projects custom-role
    /// messages into user text) by default; callers may override. Mirrors the
    /// TS wiring where the harness composes its converter into `AgentLoopConfig`.
    pub to_provider_messages: Option<ConvertToLlm>,

    /// Optional entry projectors folded into the per-turn context build. Maps a
    /// `custom` entry's `custom_type` → the messages to splice into the context.
    /// Mirrors TS `entryProjectors`.
    pub entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,

    /// Optional emitter override. When `Some`, the harness passes this emitter
    /// to `run_agent_loop` so callers (e.g. the TUI) can observe `AgentEvent`s
    /// live as a run unfolds. `None` (the default) preserves the discard
    /// behavior — the harness only surfaces `RunStart`/`RunEnd` on its own bus.
    pub agent_emitter: Option<Arc<dyn rpi_agent::AgentEmitter>>,

    // ---- B3b additions: the three exists-but-`None` `AgentLoopConfig` hooks --
    //
    // `AgentLoopConfig` already has `before_tool_call`/`after_tool_call`/
    // `transform_context` slots (pi-agent/hooks.rs), but the harness hardcoded
    // them to `None` (agent_harness.rs:944/950-951). These option fields expose
    // them on `AgentHarnessOptions` so a host (pi-cli, via rpi-extensions
    // adapters) can populate them; the harness forwards them into the config it
    // builds per run (snapshot_config + the config build site).
    /// `(context, signal) -> Option<BeforeToolCallResult>` — can block a tool
    /// call before it runs. Mirrors `AgentLoopConfig.before_tool_call`.
    pub before_tool_call: Option<BeforeToolCall>,

    /// `(context, signal) -> Option<AfterToolCallResult>` — overrides fields of
    /// an executed tool result before `tool_execution_end`/`MessageEnd`. Mirrors
    /// `AgentLoopConfig.after_tool_call`.
    pub after_tool_call: Option<AfterToolCall>,

    /// `(messages, signal) -> AgentMessage[]` — optional pre-convert transform
    /// at the `AgentMessage` level (context-window pruning, external injection).
    /// Mirrors `AgentLoopConfig.transform_context`.
    pub transform_context: Option<TransformContext>,

    /// Caller-supplied context-entry transforms applied AFTER the default
    /// compaction-boundary transform during the per-turn `build_session_context`.
    /// Previously the harness hardcoded `Vec::new()` at the `build_opts` site
    /// (agent_harness.rs:520); this field lets a host inject transforms (the
    /// pi `context`/`transform_context` event folds here too).
    pub entry_transforms: Vec<ContextEntryTransform>,

    // ---- B4: per-call provider hooks ---------------------------------------
    //
    // `ProviderHooks` (defined in rpi-ai, cycle-free) is a sidecar trait the
    // harness fires inside its `StreamFn` closure before each `stream_simple`
    // call. This lets an extension's `before_provider_request` /
    // `before_provider_headers` / `after_provider_response` `on()` handlers
    // patch the live `SimpleStreamOptions` per call (e.g. rotate a header,
    // inject a request id) — NOT once per run at the config-build site (that
    // would not be equivalent to pi's `beforeRequest`, which runs before every
    // provider call). `None` (default) = no hooks; the harness code path stays
    // uniform via `NoopProviderHooks`.
    pub provider_hooks: Option<Arc<dyn ProviderHooks>>,
}

impl Default for AgentHarnessOptions {
    fn default() -> Self {
        Self {
            model: Model::new(
                "faux".to_string(),
                "Faux".to_string(),
                rpi_ai::Api::AnthropicMessages,
                "faux".to_string(),
                "https://example.test".to_string(),
            ),
            thinking_level: ThinkingLevel::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: AgentHarnessStreamOptions::default(),
            retry: RetryPolicy::default(),
            compaction: CompactionSettings::default(),
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session: Session::new(
                Arc::new(crate::session::memory::InMemorySessionStorage::new(
                    crate::session::types::SessionMetadata {
                        id: "default".into(),
                        created_at: 0,
                        parent_session_id: None,
                    },
                    Arc::new(crate::session::memory::SystemClock),
                    Arc::new(crate::session::session::DefaultIdGenerator::new()),
                )),
                None,
            ),
            allow_existing_session: false,
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: BTreeMap::new(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RetryPolicy;

    #[test]
    fn retry_policy_allows_ten_retries_by_default() {
        let retry = RetryPolicy::default();
        assert!(retry.enabled);
        assert_eq!(retry.max_retries, 10);
    }

    #[test]
    fn header_overlays_and_removals_use_case_insensitive_names() {
        use std::collections::BTreeMap;
        let mut overlay = super::AgentHarnessStreamOptions {
            headers: Some(BTreeMap::from([(
                "authorization".into(),
                "Bearer Override".into(),
            )])),
            metadata: Some(BTreeMap::from([(
                "tenant".into(),
                "metadata override".into(),
            )])),
            ..Default::default()
        };
        let base = rpi_ai::SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer Base".into(),
            )])),
            metadata: Some(BTreeMap::from([("Tenant".into(), "metadata base".into())])),
            ..Default::default()
        };
        let merged = overlay.merge_into(base);
        assert_eq!(merged.headers, overlay.headers);
        assert_eq!(
            merged.metadata.unwrap().len(),
            2,
            "metadata names retain their case-sensitive semantics"
        );
        super::AgentHarnessStreamOptionsPatch {
            headers: Some(BTreeMap::from([("AUTHORIZATION".into(), None)])),
            ..Default::default()
        }
        .apply(&mut overlay);
        assert_eq!(overlay.headers, None);
        assert_eq!(overlay.metadata.unwrap().len(), 1);
    }

    #[test]
    fn thinking_budgets_reach_the_provider_options() {
        let budgets = rpi_ai::ThinkingBudgets {
            high: Some(24576),
            ..Default::default()
        };
        let options = super::AgentHarnessStreamOptions {
            thinking_budgets: Some(budgets),
            ..Default::default()
        };
        let merged = options.merge_into(rpi_ai::SimpleStreamOptions::default());
        assert_eq!(merged.thinking_budgets, Some(budgets));
        let unset = super::AgentHarnessStreamOptions::default().merge_into(merged);
        assert_eq!(
            unset.thinking_budgets,
            Some(budgets),
            "unset keeps the caller's budgets"
        );
    }

    #[test]
    fn stream_idle_timeout_survives_harness_merge_and_patches() {
        use std::time::Duration;
        let mut options = super::AgentHarnessStreamOptions {
            stream_idle_timeout: Some(Duration::from_secs(17)),
            ..Default::default()
        };
        let merged = options.merge_into(rpi_ai::SimpleStreamOptions::default());
        assert_eq!(merged.stream_idle_timeout(), Some(Duration::from_secs(17)));
        super::AgentHarnessStreamOptionsPatch {
            stream_idle_timeout: Some(Some(Duration::ZERO)),
            ..Default::default()
        }
        .apply(&mut options);
        assert_eq!(options.merge_into(merged).stream_idle_timeout(), None);
    }
}
