//! Mirrors `packages/agent/src/harness/session/context.ts` — building the
//! provider-facing [`SessionContext`] (messages + derived state) from a branch's
//! entry path.
//!
//! The authoritative port of `context.ts`. `compaction/tokens.rs` held a minimal
//! no-options copy pulled forward for `tokensBefore`; that copy now delegates
//! here (`build_session_context`) so the two never diverge.
//!
//! Shape notes:
//! - `defaultContextEntryTransform` keeps the **last** `compaction` entry plus
//!   everything after it; with no compaction, keeps all entries. This is the
//!   "resume from the latest summary" boundary the loop feeds the model.
//! - `sessionEntryToContextMessages` projects each entry to zero or more
//!   [`AgentMessage`]s: messages pass through (deferred assistant handles are
//!   dropped — they are placeholders, not model-visible content); a compaction
//!   entry expands to its `compactionSummary` message plus the retained tail; a
//!   non-empty `branch_summary` expands to a `branchSummary` message; a `custom`
//!   entry expands via the registered projector (or nothing).
//! - `deriveSessionContextState` walks the FULL path (not the truncated context
//!   entries) so `thinking_level`/`model`/`active_tool_names` reflect the latest
//!   change even when it fell before a compaction boundary.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::StopReason;

use crate::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::session::types::{
    is_tool_program_body_type, BranchSummaryEntry, CompactionEntry, CustomEntry, Entry,
};

/// `SessionContext` — the provider-facing snapshot built from a branch path.
/// Mirrors TS `SessionContext`. `model` carries `(provider, model_id)`.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
    pub active_tool_names: Option<Vec<String>>,
}

/// `ContextEntryTransform` — a caller-supplied transform applied to the context
/// entries AFTER the default compaction-boundary transform. Reserved native body
/// archives are not hook input; indexes refer to the filtered semantic slice.
/// Ordinary extension custom entries remain in their original order. Mirrors TS
/// `ContextEntryTransform`. `Arc` so it can live in an options bag.
pub type ContextEntryTransform = Arc<dyn Fn(&[Entry]) -> Vec<Entry> + Send + Sync>;

/// `CustomEntryContextMessageProjector` — projects a `custom` entry to context
/// messages. Mirrors TS `CustomEntryContextMessageProjector`. Receives
/// `(entry, index, entries)` for parity with the TS signature (most projectors
/// read only `entry`). Returns the messages to splice in (empty = omit).
pub type CustomEntryContextMessageProjector =
    Arc<dyn Fn(&CustomEntry, usize, &[Entry]) -> Vec<AgentMessage> + Send + Sync>;

/// `SessionContextBuildOptions`. Mirrors TS `SessionContextBuildOptions`.
#[derive(Clone, Default)]
pub struct SessionContextBuildOptions {
    pub entry_transforms: Vec<ContextEntryTransform>,
    pub entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
}

impl std::fmt::Debug for SessionContextBuildOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContextBuildOptions")
            .field("entry_transforms", &self.entry_transforms.len())
            .field(
                "entry_projectors",
                &self.entry_projectors.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// `defaultContextEntryTransform` — keep the last `compaction` entry + everything
/// after it; if no compaction, keep all entries. Mirrors
/// `defaultContextEntryTransform`.
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    default_context_entries(path_entries)
        .iter()
        .filter(|entry| !is_body_entry(entry))
        .cloned()
        .collect()
}

fn default_context_entries(path_entries: &[Entry]) -> &[Entry] {
    let start = path_entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction(_)))
        .unwrap_or(0);
    &path_entries[start..]
}

fn is_body_entry(entry: &Entry) -> bool {
    matches!(entry, Entry::Custom(custom) if is_tool_program_body_type(&custom.custom_type))
}

fn context_entries<'a>(
    path_entries: &'a [Entry],
    options: &SessionContextBuildOptions,
) -> std::borrow::Cow<'a, [Entry]> {
    let slice = default_context_entries(path_entries);
    // Direct borrowed builders may receive full resident history. Filter before
    // cloning anything or exposing the slice to user transforms/projectors.
    let mut entries = if slice.iter().any(is_body_entry) {
        std::borrow::Cow::Owned(
            slice
                .iter()
                .filter(|entry| !is_body_entry(entry))
                .cloned()
                .collect::<Vec<_>>(),
        )
    } else {
        std::borrow::Cow::Borrowed(slice)
    };
    for transform in &options.entry_transforms {
        let mut transformed = transform(&entries);
        transformed.retain(|entry| !is_body_entry(entry));
        entries = std::borrow::Cow::Owned(transformed);
    }
    entries
}

/// Build the default context by consuming an owned branch path.
///
/// The borrowed builder remains the compatibility path for callers
/// that need transforms/projectors. The harness's ordinary path already owns the
/// `Vec<Entry>` returned by storage, so cloning every message out of those entries
/// is unnecessary when no user hook can observe the intermediate entries. In that
/// case move ordinary messages and compaction retained-tail messages directly into
/// the provider context. Derived state is computed before consuming the path so it
/// still observes the full branch, including entries before the latest compaction.
pub(crate) fn build_session_context_owned(
    path_entries: Vec<Entry>,
    options: &SessionContextBuildOptions,
) -> SessionContext {
    // A transform or projector receives entry references and can inspect indexes
    // and the complete slice. Keep the borrowed implementation for those cases;
    // this also ensures stateful hook output is never cached or otherwise reused.
    if !options.entry_transforms.is_empty() || !options.entry_projectors.is_empty() {
        return build_session_context(&path_entries, options);
    }

    let diagnostics = std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
    let started = diagnostics.then(Instant::now);
    let path_entry_count = path_entries.len();
    let mut ctx = derive_session_context_state(path_entries.iter().rev());
    let start = path_entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction(_)))
        .unwrap_or(0);
    let mut messages = Vec::new();

    for entry in path_entries.into_iter().skip(start) {
        match entry {
            Entry::Message(message) => {
                if !matches!(
                    &message.message,
                    AgentMessage::Assistant(assistant)
                        if assistant.stop_reason == StopReason::Deferred
                ) {
                    messages.push(message.message);
                }
            }
            Entry::Compaction(compaction) => {
                messages.push(create_compaction_summary_message(
                    &compaction.summary,
                    compaction.tokens_before,
                    compaction.base.timestamp,
                ));
                messages.extend(compaction.retained_tail);
            }
            Entry::BranchSummary(summary) if !summary.summary.is_empty() => {
                messages.push(create_branch_summary_message(
                    &summary.summary,
                    &summary.from_id,
                    summary.base.timestamp,
                ));
            }
            // Custom entries have no registered projector on this fast path;
            // all other entry kinds are state-only and model-invisible.
            _ => {}
        }
    }
    ctx.messages = messages;
    if let Some(started) = started {
        eprintln!(
            "RPI_CONTEXT_DIAGNOSTICS path_entries={} context_entries={} messages={} build_ns={}",
            path_entry_count,
            path_entry_count.saturating_sub(start),
            ctx.messages.len(),
            started.elapsed().as_nanos().min(u64::MAX as u128),
        );
    }
    ctx
}

/// Build the default context directly from a validated branch walk. The state
/// owns the entries, so only model-visible messages and retained-tail messages
/// are cloned; old entries before the latest compaction are never cloned.
pub(crate) fn build_session_context_from_borrowed(path_entries: &[&Entry]) -> SessionContext {
    let diagnostics = std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
    let started = diagnostics.then(Instant::now);
    let mut ctx = derive_session_context_state(path_entries.iter().copied().rev());
    let start = path_entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction(_)))
        .unwrap_or(0);
    let mut messages = Vec::new();
    for entry in path_entries.iter().skip(start).copied() {
        match entry {
            Entry::Message(message) => {
                if !matches!(
                    &message.message,
                    AgentMessage::Assistant(assistant)
                        if assistant.stop_reason == StopReason::Deferred
                ) {
                    messages.push(message.message.clone());
                }
            }
            Entry::Compaction(compaction) => {
                messages.push(create_compaction_summary_message(
                    &compaction.summary,
                    compaction.tokens_before,
                    compaction.base.timestamp,
                ));
                messages.extend(compaction.retained_tail.iter().cloned());
            }
            Entry::BranchSummary(summary) if !summary.summary.is_empty() => {
                messages.push(create_branch_summary_message(
                    &summary.summary,
                    &summary.from_id,
                    summary.base.timestamp,
                ));
            }
            _ => {}
        }
    }
    ctx.messages = messages;
    if let Some(started) = started {
        eprintln!(
            "RPI_CONTEXT_DIAGNOSTICS path_entries={} context_entries={} messages={} build_ns={}",
            path_entries.len(),
            path_entries.len().saturating_sub(start),
            ctx.messages.len(),
            started.elapsed().as_nanos().min(u64::MAX as u128),
        );
    }
    ctx
}

/// `buildContextEntries` — apply the default transform, then each caller
/// transform in order. Mirrors TS `buildContextEntries`.
pub fn build_context_entries(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<Entry> {
    context_entries(path_entries, options).into_owned()
}

/// `sessionEntryToContextMessages` — project one entry to context messages.
/// Mirrors TS `sessionEntryToContextMessages`.
pub fn session_entry_to_context_messages(
    entry: &Entry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match entry {
        Entry::Message(m) => {
            if let AgentMessage::Assistant(a) = &m.message {
                if a.stop_reason == StopReason::Deferred {
                    return Vec::new();
                }
            }
            vec![m.message.clone()]
        }
        Entry::Compaction(c) => compaction_to_context_messages(c),
        Entry::BranchSummary(b) => branch_summary_to_context_messages(b),
        Entry::Custom(c) => custom_to_context_messages(c, index, entries, options),
        // model_change / thinking_level_change / active_tools_change → no context
        // message (they only shape the derived state).
        _ => Vec::new(),
    }
}

/// `createCompactionSummaryMessage(entry.summary, entry.tokensBefore, entry.timestamp)`
/// plus the retained tail. Mirrors the TS `compaction` branch.
fn compaction_to_context_messages(c: &CompactionEntry) -> Vec<AgentMessage> {
    let mut out = Vec::with_capacity(c.retained_tail.len() + 1);
    out.push(create_compaction_summary_message(
        &c.summary,
        c.tokens_before,
        c.base.timestamp,
    ));
    out.extend(c.retained_tail.iter().cloned());
    out
}

/// `createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp)` —
/// only when `entry.summary` is non-empty (mirrors TS `entry.summary && [...]`).
fn branch_summary_to_context_messages(b: &BranchSummaryEntry) -> Vec<AgentMessage> {
    if b.summary.is_empty() {
        return Vec::new();
    }
    vec![create_branch_summary_message(
        &b.summary,
        &b.from_id,
        b.base.timestamp,
    )]
}

/// Custom entry → registered projector's messages, or `[]` when no projector is
/// registered for `entry.customType`. Mirrors TS `entryProjectors?.[customType] ?? []`.
fn custom_to_context_messages(
    c: &CustomEntry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    if is_tool_program_body_type(&c.custom_type) {
        return Vec::new();
    }
    match options.entry_projectors.get(&c.custom_type) {
        Some(projector) => projector(c, index, entries),
        None => Vec::new(),
    }
}

/// `deriveSessionContextState` — inspect the full path (not truncated context
fn derive_session_context_state<'a>(
    path_entries: impl DoubleEndedIterator<Item = &'a Entry>,
) -> SessionContext {
    let mut thinking_level = None;
    let mut model: Option<(String, String)> = None;
    let mut active_tool_names: Option<Vec<String>> = None;
    for entry in path_entries {
        match entry {
            Entry::ThinkingLevel(t) if thinking_level.is_none() => {
                thinking_level = Some(t.thinking_level.clone());
            }
            Entry::ModelChange(m) if model.is_none() => {
                model = Some((m.provider.clone(), m.model_id.clone()));
            }
            Entry::Message(m) if model.is_none() => {
                if let AgentMessage::Assistant(a) = &m.message {
                    model = Some((a.provider.as_str().to_string(), a.model.clone()));
                }
            }
            Entry::ActiveTools(a) if active_tool_names.is_none() => {
                active_tool_names = Some(a.active_tool_names.clone());
            }
            _ => {}
        }
        if thinking_level.is_some() && model.is_some() && active_tool_names.is_some() {
            break;
        }
    }
    SessionContext {
        messages: Vec::new(),
        thinking_level: thinking_level.unwrap_or_else(|| "off".to_string()),
        model,
        active_tool_names,
    }
}

/// `buildSessionContext(pathEntries, options)` — derive state from the full path,
/// build context entries (default transform + caller transforms), flatMap each
/// to context messages. Mirrors TS `buildSessionContext`.
pub fn build_session_context(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> SessionContext {
    let diagnostics = std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
    let started = diagnostics.then(Instant::now);
    let mut ctx = derive_session_context_state(path_entries.iter().rev());
    let context_entries = context_entries(path_entries, options);
    let context_entry_count = context_entries.len();
    let mut messages = Vec::new();
    for (index, entry) in context_entries.iter().enumerate() {
        messages.extend(session_entry_to_context_messages(
            entry,
            index,
            &context_entries,
            options,
        ));
    }
    ctx.messages = messages;
    if let Some(started) = started {
        eprintln!(
            "RPI_CONTEXT_DIAGNOSTICS path_entries={} context_entries={} messages={} build_ns={}",
            path_entries.len(),
            context_entry_count,
            ctx.messages.len(),
            started.elapsed().as_nanos().min(u64::MAX as u128),
        );
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::AssistantMessage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    fn assistant(text: &str) -> AgentMessage {
        let mut a = AssistantMessage::empty(
            rpi_ai::types::Api::Faux,
            "anthropic",
            "claude-sonnet-4-5",
            1,
        );
        a.content = vec![rpi_ai::types::Content::text(text)];
        a.stop_reason = rpi_ai::types::StopReason::Stop;
        AgentMessage::Assistant(Box::new(a))
    }

    /// Build a message entry with a given id/parent (storage fields stamped
    /// minimally — only what `build_session_context` reads).
    fn msg_entry(id: &str, parent: Option<&str>, message: AgentMessage, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, MessageEntry};
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: id.into(),
                seq,
                parent_id: parent.map(|s| s.into()),
                timestamp: seq as i64,
            },
            message,
            terminate: None,
        })
    }

    fn msg_entry_p(id: &str, parent: &str, message: AgentMessage, seq: u64) -> Entry {
        msg_entry(id, Some(parent), message, seq)
    }

    fn compaction_entry(
        id: &str,
        parent: &str,
        summary: &str,
        tail: Vec<AgentMessage>,
        seq: u64,
    ) -> Entry {
        use crate::session::types::{CompactionEntry, EntryBase};
        Entry::Compaction(CompactionEntry {
            base: EntryBase {
                entry_type: "compaction".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            summary: summary.into(),
            retained_tail: tail,
            tokens_before: 100,
            details: None,
            usage: None,
        })
    }

    fn model_change(id: &str, parent: &str, provider: &str, model_id: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, ModelChangeEntry};
        Entry::ModelChange(ModelChangeEntry {
            base: EntryBase {
                entry_type: "model_change".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    fn thinking_level(id: &str, parent: &str, level: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, ThinkingLevelEntry};
        Entry::ThinkingLevel(ThinkingLevelEntry {
            base: EntryBase {
                entry_type: "thinking_level_change".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            thinking_level: level.into(),
        })
    }

    fn branch_summary_entry(
        id: &str,
        parent: &str,
        from_id: &str,
        summary: &str,
        seq: u64,
    ) -> Entry {
        use crate::session::types::{BranchSummaryEntry, EntryBase};
        Entry::BranchSummary(BranchSummaryEntry {
            base: EntryBase {
                entry_type: "branch_summary".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            from_id: from_id.into(),
            summary: summary.into(),
            details: None,
            usage: None,
        })
    }

    fn custom_entry(id: &str, parent: &str, custom_type: &str, data: &str, seq: u64) -> Entry {
        use crate::session::types::{CustomEntry, EntryBase};
        Entry::Custom(CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            custom_type: custom_type.into(),
            data: Some(serde_json::Value::String(data.into())),
        })
    }

    #[test]
    fn latest_state_wins_across_compaction_assistants_and_empty_tool_selection() {
        use crate::session::types::{ActiveToolsEntry, EntryBase};
        let active = |id: &str, parent: &str, tools: Vec<String>, seq| {
            Entry::ActiveTools(ActiveToolsEntry {
                base: EntryBase {
                    entry_type: "active_tools_change".into(),
                    id: id.into(),
                    seq,
                    parent_id: Some(parent.into()),
                    timestamp: seq as i64,
                },
                active_tool_names: tools,
            })
        };
        let mut entries = vec![
            model_change("model", "root", "old", "old-model", 1),
            thinking_level("thinking", "model", "high", 2),
            active("tools", "thinking", vec!["read".into()], 3),
            compaction_entry(
                "compact",
                "tools",
                "summary",
                vec![assistant("retained")],
                4,
            ),
        ];
        let old = derive_session_context_state(entries.iter().rev());
        assert_eq!(old.model, Some(("old".into(), "old-model".into())));
        assert_eq!(old.thinking_level, "high");
        assert_eq!(old.active_tool_names, Some(vec!["read".into()]));
        entries.extend([
            msg_entry_p("answer", "compact", assistant("latest"), 5),
            active("empty-tools", "answer", vec![], 6),
            thinking_level("off", "empty-tools", "off", 7),
        ]);
        let latest = derive_session_context_state(entries.iter().rev());
        assert_eq!(
            latest.model,
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(latest.thinking_level, "off");
        assert_eq!(latest.active_tool_names, Some(vec![]));
        entries.push(model_change("switch", "off", "new", "new-model", 8));
        assert_eq!(
            derive_session_context_state(entries.iter().rev()).model,
            Some(("new".into(), "new-model".into()))
        );
    }

    #[test]
    fn starts_at_latest_compaction_and_materializes_retained_tail() {
        let entries = vec![
            msg_entry("old", None, user("old"), 1),
            compaction_entry(
                "compact",
                "old",
                "summary",
                vec![user("retained"), assistant("answer")],
                2,
            ),
            model_change("model", "compact", "openai", "gpt-5", 3),
            thinking_level("thinking", "model", "high", 4),
            msg_entry_p("tail", "thinking", user("tail"), 5),
        ];
        let borrowed = build_session_context(&entries, &SessionContextBuildOptions::default());
        assert_eq!(
            borrowed
                .messages
                .iter()
                .map(|m| m.role().as_str().to_string())
                .collect::<Vec<_>>(),
            ["compactionSummary", "user", "assistant", "user"]
        );
        assert_eq!(
            borrowed.model,
            Some(("openai".to_string(), "gpt-5".to_string()))
        );
        assert_eq!(borrowed.thinking_level, "high");
        assert_eq!(borrowed.active_tool_names, None);

        let projected = build_session_context_from_borrowed(&entries.iter().collect::<Vec<_>>());
        assert_eq!(projected.messages, borrowed.messages);
        assert_eq!(projected.model, borrowed.model);
        assert_eq!(projected.thinking_level, borrowed.thinking_level);
        assert_eq!(projected.active_tool_names, borrowed.active_tool_names);

        let owned = build_session_context_owned(entries, &SessionContextBuildOptions::default());
        assert_eq!(owned.messages, borrowed.messages);
        assert_eq!(owned.thinking_level, borrowed.thinking_level);
        assert_eq!(owned.model, borrowed.model);
        assert_eq!(owned.active_tool_names, borrowed.active_tool_names);
    }

    #[test]
    fn owned_default_path_preserves_deferred_tail_summary_and_state() {
        let mut deferred =
            AssistantMessage::empty(rpi_ai::types::Api::Faux, "anthropic", "claude", 6);
        deferred.stop_reason = StopReason::Deferred;
        deferred.deferred = Some(rpi_ai::types::DeferredHandle {
            provider: "anthropic".into(),
            model_id: "claude".into(),
            api: "anthropic-messages".into(),
            id: "response-2".into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        });
        let entries = vec![
            model_change("old-model", "root", "old", "old-model", 1),
            thinking_level("old-thinking", "old-model", "high", 2),
            {
                use crate::session::types::{ActiveToolsEntry, EntryBase};
                Entry::ActiveTools(ActiveToolsEntry {
                    base: EntryBase {
                        entry_type: "active_tools_change".into(),
                        id: "old-tools".into(),
                        seq: 3,
                        parent_id: Some("old-thinking".into()),
                        timestamp: 3,
                    },
                    active_tool_names: vec!["read".into()],
                })
            },
            compaction_entry(
                "compact",
                "old-tools",
                "summary",
                vec![user("retained"), assistant("answer")],
                4,
            ),
            model_change("new-model", "compact", "anthropic", "claude", 5),
            msg_entry_p(
                "deferred",
                "new-model",
                AgentMessage::Assistant(Box::new(deferred)),
                6,
            ),
            {
                use crate::session::types::{ActiveToolsEntry, EntryBase};
                Entry::ActiveTools(ActiveToolsEntry {
                    base: EntryBase {
                        entry_type: "active_tools_change".into(),
                        id: "empty-tools".into(),
                        seq: 7,
                        parent_id: Some("deferred".into()),
                        timestamp: 7,
                    },
                    active_tool_names: vec![],
                })
            },
            thinking_level("off", "empty-tools", "off", 8),
            msg_entry_p("tail", "off", user("tail"), 9),
        ];
        let borrowed = build_session_context(&entries, &SessionContextBuildOptions::default());
        assert_eq!(
            borrowed
                .messages
                .iter()
                .map(|message| message.role().as_str().to_string())
                .collect::<Vec<_>>(),
            ["compactionSummary", "user", "assistant", "user"]
        );
        assert_eq!(borrowed.model, Some(("anthropic".into(), "claude".into())));
        assert_eq!(borrowed.thinking_level, "off");
        assert_eq!(borrowed.active_tool_names, Some(vec![]));

        let owned = build_session_context_owned(entries, &SessionContextBuildOptions::default());
        assert_eq!(owned.messages, borrowed.messages);
        assert_eq!(owned.model, borrowed.model);
        assert_eq!(owned.thinking_level, borrowed.thinking_level);
        assert_eq!(owned.active_tool_names, borrowed.active_tool_names);
    }

    #[test]
    fn owned_fallback_preserves_transform_and_projector_observation() {
        let transform_calls = Arc::new(AtomicUsize::new(0));
        let projector_calls = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let transform: ContextEntryTransform = {
            let calls = Arc::clone(&transform_calls);
            Arc::new(move |entries: &[Entry]| {
                calls.fetch_add(1, Ordering::SeqCst);
                entries.to_vec()
            })
        };
        let projector: CustomEntryContextMessageProjector = {
            let calls = Arc::clone(&projector_calls);
            let observations = Arc::clone(&observations);
            Arc::new(move |_entry: &CustomEntry, index, entries| {
                calls.fetch_add(1, Ordering::SeqCst);
                observations.lock().unwrap().push((
                    index,
                    entries.len(),
                    entries.first().map(|entry| entry.id().to_string()),
                ));
                vec![user("projected")]
            })
        };
        let mut projectors = BTreeMap::new();
        projectors.insert("note".into(), projector);
        let options = SessionContextBuildOptions {
            entry_transforms: vec![transform],
            entry_projectors: projectors,
        };
        let entries = vec![
            compaction_entry("compact", "root", "summary", vec![], 1),
            custom_entry("custom", "compact", "note", "data", 2),
            msg_entry_p("tail", "custom", user("tail"), 3),
        ];

        let context = build_session_context_owned(entries, &options);
        assert_eq!(transform_calls.load(Ordering::SeqCst), 1);
        assert_eq!(projector_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *observations.lock().unwrap(),
            vec![(1, 3, Some("compact".into()))]
        );
        assert_eq!(
            context
                .messages
                .iter()
                .map(|message| message.role().as_str().to_string())
                .collect::<Vec<_>>(),
            ["compactionSummary", "user", "user"]
        );
    }
    #[test]
    fn applies_caller_transforms_after_compaction_boundary() {
        let entries = vec![
            compaction_entry("compact", "old", "summary", vec![], 2),
            branch_summary_entry("branch", "compact", "abandoned", "branch summary", 3),
            msg_entry_p("tail", "branch", user("tail"), 4),
        ];
        let drop_compaction: ContextEntryTransform = Arc::new(|ctx_entries: &[Entry]| {
            ctx_entries
                .iter()
                .filter(|e| !matches!(e, Entry::Compaction(_)))
                .cloned()
                .collect()
        });
        let opts = SessionContextBuildOptions {
            entry_transforms: vec![drop_compaction],
            entry_projectors: BTreeMap::new(),
        };
        let ctx = build_session_context(&entries, &opts);
        assert_eq!(
            ctx.messages
                .iter()
                .map(|m| m.role().as_str().to_string())
                .collect::<Vec<_>>(),
            ["branchSummary", "user"]
        );
    }

    #[test]
    fn projects_custom_entries_and_omits_deferred_assistant() {
        // Deferred assistant handle → dropped (not model-visible).
        let mut deferred = AssistantMessage::empty(rpi_ai::types::Api::Faux, "openai", "gpt-5", 1);
        deferred.stop_reason = rpi_ai::types::StopReason::Deferred;
        deferred.deferred = Some(rpi_ai::types::DeferredHandle {
            provider: "openai".into(),
            model_id: "gpt-5".into(),
            api: "openai-responses".into(),
            id: "response-1".into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        });
        let entries = vec![
            msg_entry("user", None, user("hello"), 1),
            msg_entry_p(
                "deferred",
                "user",
                AgentMessage::Assistant(Box::new(deferred)),
                2,
            ),
            custom_entry("custom", "deferred", "note", "project me", 3),
        ];
        let projector: CustomEntryContextMessageProjector = Arc::new(|c: &CustomEntry, _, _| {
            vec![user(&format!(
                "note: {}",
                c.data
                    .clone()
                    .unwrap_or(serde_json::Value::Null)
                    .to_string()
                    .trim_matches('"')
            ))]
        });
        let mut projectors = BTreeMap::new();
        projectors.insert("note".to_string(), projector);
        let opts = SessionContextBuildOptions {
            entry_transforms: vec![],
            entry_projectors: projectors,
        };
        let ctx = build_session_context(&entries, &opts);
        assert_eq!(
            ctx.messages
                .iter()
                .map(|m| m.role().as_str().to_string())
                .collect::<Vec<_>>(),
            ["user", "user"]
        );
    }
}
