//! `AgentSession` — the coding-agent product layer over the harness.
//!
//! Mirrors the role of `packages/coding-agent/src/core/agent-session.ts`: one
//! object the CLI/TUI holds that unifies prompt/continue, queueing,
//! model/thinking mutation, scoped models, compaction + retry, export,
//! session switching / fork, usage + context stats, and name/label facts.
//!
//! Design notes (deliberate, vs the TS original):
//!
//! - **Composition, not inheritance.** TS `AgentSession` *is* the harness plus
//!   a pile of product concerns. Here [`AgentSession`] owns an `AgentHarness`
//!   and adds the product surface; the harness stays usable on its own for
//!   SDK consumers who do not want the product layer. This keeps the
//!   `rpi-harness` crate free of CLI concerns (provider catalogs, settings
//!   paths, export formats).
//! - **Sync-friendly handles.** The TUI key loop is a blocking thread, so the
//!   mutating surface that the loop touches (`set_thinking_level`,
//!   `scoped_models`) works off cached state or spawns onto the runtime; the
//!   async surface (`prompt`, `compact`, `export`) is `async fn`.
//! - **Retry/compaction policy lives in the harness.** The session exposes
//!   `retry()`/`compact()` as intent, and the harness applies its own
//!   `RetryPolicy`/`CompactionSettings`. TS keeps the auto-retry driver in the
//!   session; we keep it in the harness so non-CLI callers get it for free.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rpi_agent::AgentMessage;
use rpi_ai::types::{AssistantMessage, Content, ImageContent, Usage};
use rpi_ai::{Model, ThinkingLevel};
use rpi_harness::agent_harness::{
    AgentHarness, AgentLane, HarnessRunOutcome, LaneSnapshot, RunResult,
};
use rpi_harness::result::{HarnessError, HarnessResult};
use rpi_harness::session::types::{Entry, EntryOrder, EntryQuery};

use crate::export::{export_session, ExportFormat};

/// Where a session content lives — used by export naming and `--continue`.
#[derive(Debug, Clone)]
pub struct SessionDescriptor {
    pub id: String,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
}

/// A snapshot of usage + context pressure for the footer/status line.
#[derive(Debug, Clone, Default)]
pub struct SessionUsageStats {
    pub message_count: u64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub uncached_tokens: i64,
    pub cost_total: f64,
}

impl SessionUsageStats {
    /// Fraction of the rolled-up input tokens that came from cache. `None` when
    /// no input tokens have been recorded yet (avoids a bogus `0 %`).
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let input = self.cached_tokens + self.uncached_tokens;
        if input <= 0 {
            return None;
        }
        Some(self.cached_tokens as f64 / input as f64)
    }
}

/// One scoped-model entry: the routing id plus its human label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedModel {
    pub id: String,
    pub label: String,
}

impl ScopedModel {
    /// Parse the `provider/model[:thinking]` form used by `--model` and
    /// `settings.json#scopedModels`.
    pub fn parse(raw: &str) -> Self {
        let id = raw.trim().to_string();
        let label = id
            .rsplit('/')
            .next()
            .unwrap_or(&id)
            .split(':')
            .next()
            .unwrap_or(&id)
            .to_string();
        Self { id, label }
    }
}

/// The product-level session. Cheap to clone — the harness is behind an `Arc`.
#[derive(Clone)]
pub struct AgentSession {
    harness: Arc<AgentHarness>,
    catalog: Arc<Vec<Model>>,
    scoped_models: Arc<Vec<ScopedModel>>,
    cwd: PathBuf,
}

impl AgentSession {
    /// Wrap an existing harness. `catalog` is the resolved model catalog the
    /// session may switch within; `scoped_models` is the (possibly empty)
    /// settings-driven subset used by Ctrl+P / `/model`.
    pub fn new(
        harness: AgentHarness,
        catalog: Vec<Model>,
        scoped_models: Vec<ScopedModel>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            harness: Arc::new(harness),
            catalog: Arc::new(catalog),
            scoped_models: Arc::new(scoped_models),
            cwd: cwd.into(),
        }
    }

    /// The wrapped harness, for the call sites (TUI drain task, extension
    /// bridge) that already speak the harness API.
    pub fn harness(&self) -> &AgentHarness {
        &self.harness
    }

    /// The `Arc` handle, for spawning work onto the runtime.
    pub fn harness_arc(&self) -> Arc<AgentHarness> {
        Arc::clone(&self.harness)
    }

    /// The main-lane runner.
    pub fn lane(&self) -> Arc<dyn AgentLane> {
        self.harness.lane("main")
    }

    /// The working directory the session was opened in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The resolved model catalog.
    pub fn catalog(&self) -> &[Model] {
        &self.catalog
    }

    /// The active scoped-model set (`[]` → the full catalog is in play).
    pub fn scoped_models(&self) -> &[ScopedModel] {
        &self.scoped_models
    }

    // -- prompt / continue / queue -------------------------------------------

    /// Send a prompt with optional image attachments. Mirrors TS
    /// `AgentSession.prompt`.
    pub async fn prompt(&self, text: &str, images: Vec<ImageContent>) -> HarnessResult<RunResult> {
        self.harness.prompt_text(text, images).await
    }

    /// Send an already-built user message. Mirrors TS
    /// `AgentSession.sendUserMessage`.
    pub async fn send_message(&self, message: AgentMessage) -> HarnessResult<RunResult> {
        self.harness.prompt_message(message).await
    }

    /// Queue a steering message for the in-flight run. Mirrors TS
    /// `AgentSession.steer`.
    pub async fn steer(&self, message: AgentMessage) -> HarnessResult<()> {
        self.harness.steer(message).await?;
        Ok(())
    }

    /// Queue a follow-up message for after the in-flight run. Mirrors TS
    /// `AgentSession.followUp`.
    pub async fn follow_up(&self, message: AgentMessage) -> HarnessResult<()> {
        self.harness.follow_up(message).await?;
        Ok(())
    }

    /// Abort the in-flight run. Mirrors TS `AgentSession.abort`.
    pub async fn abort(&self) -> HarnessResult<()> {
        self.harness.abort().await?;
        Ok(())
    }

    // -- model / thinking ----------------------------------------------------

    /// The current model.
    pub async fn model(&self) -> HarnessResult<Model> {
        self.harness.get_model().await
    }

    /// Switch the model. Mirrors TS `AgentSession.setModel`.
    pub async fn set_model(&self, model: Model) -> HarnessResult<()> {
        self.harness.set_model(model).await
    }

    /// The current thinking level.
    pub async fn thinking_level(&self) -> HarnessResult<ThinkingLevel> {
        self.harness.get_thinking_level().await
    }

    /// Set the thinking level. Mirrors TS `AgentSession.setThinkingLevel`.
    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()> {
        self.harness.set_thinking_level(level).await
    }

    /// The effective model cycle order: the scoped set when configured, else
    /// the full catalog. Mirrors TS `AgentSession.getScopedModels`.
    pub fn cycle_catalog(&self) -> Vec<Model> {
        if self.scoped_models.is_empty() {
            return self.catalog.to_vec();
        }
        let mut out = Vec::new();
        for scoped in self.scoped_models.iter() {
            let bare = scoped.id.split(':').next().unwrap_or(&scoped.id);
            if let Some(model) = self.catalog.iter().find(|m| {
                m.id == bare
                    || format!("{}/{}", m.provider, m.id) == bare
                    || format!("{}/{}", m.provider, m.id) == scoped.id
            }) {
                out.push(model.clone());
            }
        }
        out
    }

    // -- compaction / retry --------------------------------------------------

    /// Explicit compaction. Mirrors TS `AgentSession.compact`.
    pub async fn compact(&self, instructions: Option<&str>) -> HarnessResult<()> {
        self.harness.compact(instructions).await?;
        Ok(())
    }

    /// Retry the last turn. The harness owns the retry policy; this re-runs the
    /// last user message when one exists. Mirrors TS `AgentSession.retry`.
    pub async fn retry(&self) -> HarnessResult<Option<RunResult>> {
        let Some(last_user) = self.last_user_message().await? else {
            return Ok(None);
        };
        self.harness.prompt_message(last_user).await.map(Some)
    }

    // -- durable reads -------------------------------------------------------

    /// The current leaf entry id.
    pub async fn tip_id(&self) -> HarnessResult<Option<String>> {
        self.harness.get_tip_id().await
    }

    /// All entries on the session, oldest first.
    pub async fn entries(&self) -> HarnessResult<Vec<Entry>> {
        self.harness
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
    }

    /// The lane read model. Mirrors the TS runtime lane snapshot.
    pub async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        self.harness.snapshot().await
    }

    /// Name/label facts. Mirrors TS `AgentSession.setName` / `setLabel`.
    pub async fn name(&self) -> HarnessResult<Option<String>> {
        self.harness.get_name().await
    }

    /// Set (or clear) the session name.
    pub async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        self.harness.set_name(name).await
    }

    /// Attach (or clear) a label on an entry.
    pub async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        self.harness.set_label(target_id, label).await
    }

    /// Read a durable session value (see `rpi_harness::session::values`).
    pub async fn value(&self, key: &str) -> HarnessResult<Option<serde_json::Value>> {
        let values = rpi_harness::session::SessionValues::load(self.harness.session())
            .await
            .map_err(|e| HarnessError::io(e.to_string()))?;
        Ok(values.get(key).cloned())
    }

    /// Persist a durable session value (latest-wins). Used for per-session
    /// display preferences so they survive resume.
    pub async fn set_value(&self, key: &str, value: serde_json::Value) -> HarnessResult<()> {
        let writer = rpi_harness::session::SessionValueWriter::new(self.harness.session().clone());
        writer
            .set(key, value)
            .await
            .map_err(|e| HarnessError::io(e.to_string()))?;
        Ok(())
    }

    /// Rolled-up usage + context stats for the footer.
    pub async fn usage_stats(&self) -> HarnessResult<SessionUsageStats> {
        let stats = self.harness.get_stats().await?;
        Ok(SessionUsageStats {
            message_count: stats.message_count,
            total_tokens: stats.total_tokens,
            cached_tokens: stats.cached_tokens,
            uncached_tokens: stats.uncached_tokens,
            cost_total: stats.cost_total,
        })
    }

    /// The summed usage across every assistant message (for `/context`).
    pub async fn total_usage(&self) -> HarnessResult<Usage> {
        let mut total = Usage::zero();
        for entry in self.entries().await? {
            if let Entry::Message(me) = entry {
                if let AgentMessage::Assistant(a) = &me.message {
                    total.input += a.usage.input;
                    total.output += a.usage.output;
                    total.cache_read += a.usage.cache_read;
                    total.cache_write += a.usage.cache_write;
                    total.total_tokens += a.usage.total_tokens;
                }
            }
        }
        Ok(total)
    }

    /// The last finalized assistant text (for `/copy`). Mirrors TS
    /// `AgentSession.getLastAssistantText`.
    pub async fn last_assistant_text(&self) -> HarnessResult<Option<String>> {
        let entries = self.entries().await?;
        for entry in entries.into_iter().rev() {
            if let Entry::Message(me) = entry {
                if let AgentMessage::Assistant(a) = me.message {
                    let text = assistant_text(&a);
                    if !text.is_empty() {
                        return Ok(Some(text));
                    }
                }
            }
        }
        Ok(None)
    }

    // -- export --------------------------------------------------------------

    /// `exportToHtml()`. Mirrors TS `AgentSession.exportToHtml`.
    pub async fn export_to_html(&self, path: &Path) -> HarnessResult<()> {
        self.export(ExportFormat::Html, path).await
    }

    /// `exportToJsonl()`. Mirrors TS `AgentSession.exportToJsonl`.
    pub async fn export_to_jsonl(&self, path: &Path) -> HarnessResult<()> {
        self.export(ExportFormat::Jsonl, path).await
    }

    /// `exportToMarkdown()` — the local `/export` default.
    pub async fn export_to_markdown(&self, path: &Path) -> HarnessResult<()> {
        self.export(ExportFormat::Markdown, path).await
    }

    /// Export to `format`, deriving the default filename from the session
    /// name/leaf when `path` is a directory.
    pub async fn export(&self, format: ExportFormat, path: &Path) -> HarnessResult<()> {
        let target = if path.is_dir() {
            path.join(self.default_export_file_name(format.clone()).await)
        } else {
            path.to_path_buf()
        };
        export_session(&self.harness, format, &target)
            .await
            .map_err(HarnessError::io)
    }

    /// The filename `/export` would write for `format`.
    pub async fn default_export_file_name(&self, format: ExportFormat) -> String {
        let extension = match format {
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Jsonl => "jsonl",
        };
        let name = self.name().await.ok().flatten().unwrap_or_default();
        if !name.is_empty() {
            return format!("{name}.{extension}");
        }
        let id = self
            .tip_id()
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "session".to_string());
        format!("{id}.{extension}")
    }

    // -- fork / tree ---------------------------------------------------------

    /// Fork the session at the current leaf into a labelled branch. The
    /// navigation machinery creates the branch; the label tags its tip.
    /// Mirrors TS `AgentSession.fork`.
    pub async fn fork(&self, label: Option<&str>) -> HarnessResult<()> {
        self.harness.navigate_tree(None, false, None, label).await?;
        Ok(())
    }

    /// Read back the terminal outcome of a run by id. Mirrors TS
    /// `AgentSession.getResult`.
    pub async fn result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        self.harness.get_result(run_id).await
    }

    // -- internals -----------------------------------------------------------

    async fn last_user_message(&self) -> HarnessResult<Option<AgentMessage>> {
        let entries = self.entries().await?;
        for entry in entries.into_iter().rev() {
            if let Entry::Message(me) = entry {
                if matches!(me.message, AgentMessage::User(_)) {
                    return Ok(Some(me.message));
                }
            }
        }
        Ok(None)
    }
}

/// The finalized text of an assistant message (text blocks only), joined with
/// blank lines. Mirrors the TS print-mode/export concat.
pub fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a run outcome is terminal (i.e. produced a final assistant message).
pub fn outcome_is_completed(outcome: &HarnessRunOutcome) -> bool {
    matches!(outcome, HarnessRunOutcome::Completed { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::Api;
    use rpi_harness::agent_harness::AgentHarness;
    use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
    use rpi_harness::session::session::{DefaultIdGenerator, Session};
    use rpi_harness::session::types::SessionMetadata;
    use rpi_harness::types::{
        AgentHarnessOptions, AgentHarnessResources, DrivingMode, HarnessToolExecution, RetryPolicy,
    };

    fn test_session() -> Session {
        Session::new(
            std::sync::Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "s-agent-session".to_string(),
                    created_at: 0,
                    parent_session_id: None,
                },
                std::sync::Arc::new(SystemClock),
                std::sync::Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        )
    }

    async fn test_harness() -> AgentHarness {
        AgentHarness::create(AgentHarnessOptions {
            model: Model::new("m", "m", Api::Faux, "faux", "http://x"),
            thinking_level: Default::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: Default::default(),
            retry: RetryPolicy::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session: test_session(),
            allow_existing_session: true,
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
        })
        .await
        .unwrap()
    }

    #[test]
    fn scoped_model_parses_provider_and_thinking_forms() {
        let m = ScopedModel::parse("anthropic/claude-opus-4-8:high");
        assert_eq!(m.id, "anthropic/claude-opus-4-8:high");
        assert_eq!(m.label, "claude-opus-4-8");

        let bare = ScopedModel::parse("gpt-5");
        assert_eq!(bare.label, "gpt-5");
    }

    #[test]
    fn cache_hit_ratio_is_none_without_input() {
        let stats = SessionUsageStats::default();
        assert!(stats.cache_hit_ratio().is_none());

        let stats = SessionUsageStats {
            cached_tokens: 75,
            uncached_tokens: 25,
            ..Default::default()
        };
        assert_eq!(stats.cache_hit_ratio(), Some(0.75));
    }

    #[test]
    fn assistant_text_joins_text_blocks_only() {
        let mut msg =
            rpi_ai::types::AssistantMessage::empty(rpi_ai::Api::AnthropicMessages, "p", "m", 0);
        msg.content = vec![
            Content::Thinking(rpi_ai::types::ThinkingContent {
                kind: rpi_ai::types::ThinkingContentType,
                thinking: "hidden".into(),
                thinking_signature: None,
                redacted: false,
            }),
            Content::Text(rpi_ai::types::TextContent {
                kind: rpi_ai::types::TextContentType,
                text: "hello".into(),
                text_signature: None,
            }),
            Content::Text(rpi_ai::types::TextContent {
                kind: rpi_ai::types::TextContentType,
                text: "world".into(),
                text_signature: None,
            }),
        ];
        assert_eq!(assistant_text(&msg), "hello\nworld");
    }

    #[tokio::test]
    async fn session_values_round_trip_through_the_product_layer() {
        let session = AgentSession::new(test_harness().await, Vec::new(), Vec::new(), ".");
        assert!(session
            .value("display.hide_thinking")
            .await
            .unwrap()
            .is_none());

        session
            .set_value("display.hide_thinking", serde_json::json!(true))
            .await
            .unwrap();
        assert_eq!(
            session.value("display.hide_thinking").await.unwrap(),
            Some(serde_json::json!(true))
        );

        // Latest-wins.
        session
            .set_value("display.hide_thinking", serde_json::json!(false))
            .await
            .unwrap();
        assert_eq!(
            session.value("display.hide_thinking").await.unwrap(),
            Some(serde_json::json!(false))
        );
    }

    #[tokio::test]
    async fn usage_stats_and_snapshot_read_from_the_empty_session() {
        let session = AgentSession::new(test_harness().await, Vec::new(), Vec::new(), ".");
        let stats = session.usage_stats().await.unwrap();
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.total_tokens, 0);
        assert!(stats.cache_hit_ratio().is_none());

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.lane, "main");
        assert!(snapshot.active.is_none());
    }

    #[tokio::test]
    async fn export_writes_every_supported_format() {
        let session = AgentSession::new(test_harness().await, Vec::new(), Vec::new(), ".");
        let dir = tempfile::tempdir().unwrap();

        for (format, ext) in [
            (ExportFormat::Markdown, "md"),
            (ExportFormat::Html, "html"),
            (ExportFormat::Jsonl, "jsonl"),
        ] {
            let name = session.default_export_file_name(format.clone()).await;
            assert!(name.ends_with(&format!(".{ext}")), "unexpected name {name}");
            let path = dir.path().join(&name);
            session.export(format, &path).await.unwrap();
            assert!(path.exists(), "{ext} export missing at {}", path.display());
        }
    }
}
