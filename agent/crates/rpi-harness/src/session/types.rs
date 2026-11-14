//! Mirrors `packages/agent/src/harness/session/types.ts` — the durable session
//! data model: `Entry`, `LaneRecord`, queries, `SessionStorage`/`SessionRepo`
//! traits, `SessionTree`, `ForkOptions`, `LogItem`, stats.
//!
//! The TS file uses discriminated unions (`type` field) with `Omit`/`Pick` to
//! derive `ProvisionedEntry` and `NewRecord`. Rust models the same with
//! `#[serde(tag=…)]` enums and a `Provisioned` marker: a provisioned entry is
//! the entry-minus-`{seq,parentId,timestamp}` — represented here by
//! [`ProvisionedEntry`] (an enum parallel to [`Entry`]) so the compiler
//! enforces "no storage-assigned fields".

use std::collections::BTreeMap;

use rpi_ai::types::{DeferredHandle, StopReason, Usage};
use serde::{Deserialize, Serialize};

use crate::error::{SessionError, SessionResult};

/// JSON value alias — mirrors `JsonValue` from the TS module. `pi-harness`
/// stores opaque app payloads as `serde_json::Value` rather than a recursive
/// Rust enum; the behavior (JSON-serializable, no symbols/functions) is the
/// same and the codec's `assert_json_serializable` keeps it sound.
pub type JsonValue = serde_json::Value;

/// Reverse of [`provisioned_into_entry`]: pull a [`ProvisionedEntry`] out of a
/// full [`Entry`] (dropping storage-assigned `seq`/`parent_id`/`timestamp`).
/// Used by the fork path, which reconstructs provisioned entries from the
/// source session's entries so the forked session can re-stamp `seq`.
pub fn provisioned_from_entry(entry: &Entry) -> ProvisionedEntry {
    let id = entry.id().to_string();
    let kind = match entry {
        Entry::Message(m) => ProvisionedKind::Message {
            message: m.message.clone(),
            terminate: m.terminate,
        },
        Entry::ModelChange(m) => ProvisionedKind::ModelChange {
            provider: m.provider.clone(),
            model_id: m.model_id.clone(),
        },
        Entry::ThinkingLevel(m) => ProvisionedKind::ThinkingLevel {
            thinking_level: m.thinking_level.clone(),
        },
        Entry::ActiveTools(m) => ProvisionedKind::ActiveTools {
            active_tool_names: m.active_tool_names.clone(),
        },
        Entry::Compaction(m) => ProvisionedKind::Compaction {
            summary: m.summary.clone(),
            retained_tail: m.retained_tail.clone(),
            tokens_before: m.tokens_before,
            details: m.details.clone(),
            usage: m.usage.clone(),
        },
        Entry::BranchSummary(m) => ProvisionedKind::BranchSummary {
            from_id: m.from_id.clone(),
            summary: m.summary.clone(),
            details: m.details.clone(),
            usage: m.usage.clone(),
        },
        Entry::Custom(m) => ProvisionedKind::Custom {
            custom_type: m.custom_type.clone(),
            data: m.data.clone(),
        },
    };
    ProvisionedEntry { id, kind }
}

/// `Exclude<StopReason, "pending"> | "deferred"`. The session persists only
/// terminal stop reasons; `pending` is a transient streaming state.
///
/// Serialized with `rename_all = "camelCase"` so `ToolUse` → `"toolUse"` matches
/// the `rpi_ai` wire convention (NOT `"tool_use"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl SessionStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStopReason::Stop => "stop",
            SessionStopReason::Length => "length",
            SessionStopReason::ToolUse => "toolUse",
            SessionStopReason::Error => "error",
            SessionStopReason::Aborted => "aborted",
            SessionStopReason::Deferred => "deferred",
        }
    }

    /// Adapt a live [`StopReason`] to the persistable set, mapping `Pending` to
    /// `Error` (a pending stop reason at persist time is an incomplete stream —
    /// recorded as an error so recovery treats it as fatal, never resumable).
    pub fn from_live(reason: StopReason) -> Self {
        match reason {
            StopReason::Stop => SessionStopReason::Stop,
            StopReason::Length => SessionStopReason::Length,
            StopReason::ToolUse => SessionStopReason::ToolUse,
            StopReason::Error => SessionStopReason::Error,
            StopReason::Aborted => SessionStopReason::Aborted,
            StopReason::Deferred => SessionStopReason::Deferred,
            StopReason::Pending => SessionStopReason::Error,
        }
    }
}

/// Stable abstraction over id generation. `Session::new` defaults to uuidv7.
/// Tests inject a deterministic counter; the codec/repo inject uuidv7 directly.
pub trait IdGenerator: Send + Sync {
    fn next(&self) -> String;
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// Shared storage-assigned fields on every entry. Mirrors TS `EntryBase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    /// Shared sequence; storage-assigned. Consecutive-seq invariant enforced by
    /// `SessionState::apply_mutation`.
    pub seq: u64,
    /// The appending lane's leaf at append time; storage-assigned. `Option` only
    /// on the fork path (see `SessionMutation::Entry { lane: Option }`).
    pub parent_id: Option<String>,
    /// Unix ms; storage-assigned.
    pub timestamp: i64,
}

/// A persisted agent message. Mirrors `MessageEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub message: rpi_agent::message::AgentMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub thinking_level: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveToolsEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub active_tool_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    pub retained_tail: Vec<rpi_agent::message::AgentMessage>,
    pub tokens_before: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub from_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

/// `Entry = MessageEntry | ModelChangeEntry | ... | CustomEntry`.
///
/// Serialized as a flat object (each arm `#[serde(flatten)]`s its `base`), so
/// the `type` discriminator lives at the top level — matching the TS wire shape.
/// `#[serde(tag="type")]` would collide with the flattened `base.type`, so
/// [`Entry`] uses a manual `Serialize`/`Deserialize` that emits/reads the flat
/// shape via [`Entry::to_flat_json`].
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Message(MessageEntry),
    ModelChange(ModelChangeEntry),
    ThinkingLevel(ThinkingLevelEntry),
    ActiveTools(ActiveToolsEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Custom(CustomEntry),
}

impl Entry {
    pub fn base(&self) -> &EntryBase {
        match self {
            Entry::Message(e) => &e.base,
            Entry::ModelChange(e) => &e.base,
            Entry::ThinkingLevel(e) => &e.base,
            Entry::ActiveTools(e) => &e.base,
            Entry::Compaction(e) => &e.base,
            Entry::BranchSummary(e) => &e.base,
            Entry::Custom(e) => &e.base,
        }
    }

    pub fn base_mut(&mut self) -> &mut EntryBase {
        match self {
            Entry::Message(e) => &mut e.base,
            Entry::ModelChange(e) => &mut e.base,
            Entry::ThinkingLevel(e) => &mut e.base,
            Entry::ActiveTools(e) => &mut e.base,
            Entry::Compaction(e) => &mut e.base,
            Entry::BranchSummary(e) => &mut e.base,
            Entry::Custom(e) => &mut e.base,
        }
    }

    pub fn id(&self) -> &str {
        &self.base().id
    }
    pub fn seq(&self) -> u64 {
        self.base().seq
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.base().parent_id.as_deref()
    }
    pub fn entry_type(&self) -> &'static str {
        match self {
            Entry::Message(_) => "message",
            Entry::ModelChange(_) => "model_change",
            Entry::ThinkingLevel(_) => "thinking_level_change",
            Entry::ActiveTools(_) => "active_tools_change",
            Entry::Compaction(_) => "compaction",
            Entry::BranchSummary(_) => "branch_summary",
            Entry::Custom(_) => "custom",
        }
    }

    /// The role of a message entry's message, or `None` for non-message entries.
    /// Used by compaction's `findValidCutPoints` / `findTurnStartIndex`.
    /// Returns an owned [`rpi_agent::message::AgentMessageRole`] (it may carry a
    /// custom role `String`, so it can't be a cheap `Copy`).
    pub fn message_role(&self) -> Option<rpi_agent::message::AgentMessageRole> {
        match self {
            Entry::Message(m) => Some(m.message.role()),
            _ => None,
        }
    }

    /// Borrow the message if this is a `Message` entry.
    pub fn as_message(&self) -> Option<&rpi_agent::message::AgentMessage> {
        match self {
            Entry::Message(m) => Some(&m.message),
            _ => None,
        }
    }

    /// Build the flat JSON object for this entry (base fields + arm-specific
    /// fields merged). Used by the manual `Serialize` and the JSONL codec.
    pub fn to_flat_json(&self) -> serde_json::Value {
        use serde_json::{Map, Value};
        let base = self.base();
        let mut map = Map::new();
        map.insert("type".into(), Value::String(self.entry_type().into()));
        map.insert("id".into(), Value::String(base.id.clone()));
        map.insert("seq".into(), Value::Number(base.seq.into()));
        match &base.parent_id {
            Some(s) => map.insert("parentId".into(), Value::String(s.clone())),
            None => map.insert("parentId".into(), Value::Null),
        };
        map.insert("timestamp".into(), Value::Number(base.timestamp.into()));

        // Serialize the arm (which flattens `base` again) and merge in only the
        // arm-specific keys (skip the 5 base keys we already wrote).
        let arm = match self {
            Entry::Message(e) => serde_json::to_value(e),
            Entry::ModelChange(e) => serde_json::to_value(e),
            Entry::ThinkingLevel(e) => serde_json::to_value(e),
            Entry::ActiveTools(e) => serde_json::to_value(e),
            Entry::Compaction(e) => serde_json::to_value(e),
            Entry::BranchSummary(e) => serde_json::to_value(e),
            Entry::Custom(e) => serde_json::to_value(e),
        };
        if let Ok(Value::Object(arm_map)) = arm {
            for (k, v) in arm_map {
                if matches!(k.as_str(), "type" | "id" | "seq" | "parentId" | "timestamp") {
                    continue;
                }
                map.insert(k, v);
            }
        }
        Value::Object(map)
    }
}

impl Serialize for Entry {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_flat_json().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        let obj = raw
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("entry is not a JSON object"))?;
        let ty = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::custom("entry missing \"type\""))?;
        let base = deserialize_base(obj).map_err(serde::de::Error::custom)?;
        let entry = match ty {
            "message" => Entry::Message(MessageEntry {
                base,
                message: serde_json::from_value(obj.get("message").cloned().ok_or_else(|| {
                    serde::de::Error::custom("message entry missing \"message\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                terminate: obj.get("terminate").and_then(|v| v.as_bool()),
            }),
            "model_change" => {
                Entry::ModelChange(ModelChangeEntry {
                    base,
                    provider: serde_json::from_value(obj.get("provider").cloned().ok_or_else(
                        || serde::de::Error::custom("model_change missing \"provider\""),
                    )?)
                    .map_err(serde::de::Error::custom)?,
                    model_id: serde_json::from_value(obj.get("modelId").cloned().ok_or_else(
                        || serde::de::Error::custom("model_change missing \"modelId\""),
                    )?)
                    .map_err(serde::de::Error::custom)?,
                })
            }
            "thinking_level_change" => Entry::ThinkingLevel(ThinkingLevelEntry {
                base,
                thinking_level: serde_json::from_value(
                    obj.get("thinkingLevel").cloned().ok_or_else(|| {
                        serde::de::Error::custom("thinking_level_change missing \"thinkingLevel\"")
                    })?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "active_tools_change" => Entry::ActiveTools(ActiveToolsEntry {
                base,
                active_tool_names: serde_json::from_value(
                    obj.get("activeToolNames").cloned().ok_or_else(|| {
                        serde::de::Error::custom("active_tools_change missing \"activeToolNames\"")
                    })?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "compaction" => {
                let retained_tail =
                    serde_json::from_value(obj.get("retainedTail").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"retainedTail\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                let summary =
                    serde_json::from_value(obj.get("summary").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"summary\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                let tokens_before =
                    serde_json::from_value(obj.get("tokensBefore").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"tokensBefore\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                Entry::Compaction(CompactionEntry {
                    base,
                    summary,
                    retained_tail,
                    tokens_before,
                    details: obj.get("details").cloned(),
                    usage: obj
                        .get("usage")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                })
            }
            "branch_summary" => Entry::BranchSummary(BranchSummaryEntry {
                base,
                from_id: serde_json::from_value(obj.get("fromId").cloned().ok_or_else(|| {
                    serde::de::Error::custom("branch_summary missing \"fromId\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                summary: serde_json::from_value(obj.get("summary").cloned().ok_or_else(|| {
                    serde::de::Error::custom("branch_summary missing \"summary\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                details: obj.get("details").cloned(),
                usage: obj
                    .get("usage")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
            }),
            "custom" => Entry::Custom(CustomEntry {
                base,
                custom_type: serde_json::from_value(
                    obj.get("customType")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("custom missing \"customType\""))?,
                )
                .map_err(serde::de::Error::custom)?,
                data: obj.get("data").cloned(),
            }),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown entry type {other}"
                )))
            }
        };
        Ok(entry)
    }
}

fn deserialize_base(obj: &serde_json::Map<String, serde_json::Value>) -> Result<EntryBase, String> {
    Ok(EntryBase {
        entry_type: obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or("missing type")?
            .to_string(),
        id: obj
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("missing id")?
            .to_string(),
        seq: obj
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or("missing seq")?,
        parent_id: obj
            .get("parentId")
            .and_then(|v| v.as_str())
            .map(String::from),
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .ok_or("missing timestamp")?,
    })
}

/// An entry without storage-assigned `seq`/`parent_id`/`timestamp` — what
/// callers hand to `append_entry`. Mirrors TS `ProvisionedEntry<TEntry>`.
///
/// We store the parts the caller DOES provide (`id` + the arm fields) and let
/// the storage layer synthesize the full [`Entry`] by stamping the storage
/// fields via [`provisioned_into_entry`].
///
/// `Serialize` is derived so the `Session` facade can run
/// `assertJsonSerializable` on the provisioned payload before commit (mirrors
/// TS `assertJsonSerializable(entry)`). The on-disk codec serializes the full
/// [`Entry`] (post-stamping), not this shape; this derive is for validation
/// only and carries no serde tag attributes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProvisionedEntry {
    pub id: String,
    pub kind: ProvisionedKind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ProvisionedKind {
    Message {
        message: rpi_agent::message::AgentMessage,
        terminate: Option<bool>,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingLevel {
        thinking_level: String,
    },
    ActiveTools {
        active_tool_names: Vec<String>,
    },
    Compaction {
        summary: String,
        retained_tail: Vec<rpi_agent::message::AgentMessage>,
        tokens_before: i64,
        details: Option<JsonValue>,
        usage: Option<Usage>,
    },
    BranchSummary {
        from_id: String,
        summary: String,
        details: Option<JsonValue>,
        usage: Option<Usage>,
    },
    Custom {
        custom_type: String,
        data: Option<JsonValue>,
    },
}

impl ProvisionedEntry {
    /// Discriminator tag for the provisioned shape (mirrors `entry.type`).
    pub fn type_tag(&self) -> &'static str {
        match &self.kind {
            ProvisionedKind::Message { .. } => "message",
            ProvisionedKind::ModelChange { .. } => "model_change",
            ProvisionedKind::ThinkingLevel { .. } => "thinking_level_change",
            ProvisionedKind::ActiveTools { .. } => "active_tools_change",
            ProvisionedKind::Compaction { .. } => "compaction",
            ProvisionedKind::BranchSummary { .. } => "branch_summary",
            ProvisionedKind::Custom { .. } => "custom",
        }
    }
}

/// Turn a [`ProvisionedEntry`] into a full [`Entry`] by stamping storage fields.
pub fn provisioned_into_entry(
    p: ProvisionedEntry,
    seq: u64,
    parent_id: Option<String>,
    timestamp: i64,
) -> Entry {
    let base = EntryBase {
        entry_type: p.type_tag().to_string(),
        id: p.id,
        seq,
        parent_id,
        timestamp,
    };
    match p.kind {
        ProvisionedKind::Message { message, terminate } => Entry::Message(MessageEntry {
            base,
            message,
            terminate,
        }),
        ProvisionedKind::ModelChange { provider, model_id } => {
            Entry::ModelChange(ModelChangeEntry {
                base,
                provider,
                model_id,
            })
        }
        ProvisionedKind::ThinkingLevel { thinking_level } => {
            Entry::ThinkingLevel(ThinkingLevelEntry {
                base,
                thinking_level,
            })
        }
        ProvisionedKind::ActiveTools { active_tool_names } => {
            Entry::ActiveTools(ActiveToolsEntry {
                base,
                active_tool_names,
            })
        }
        ProvisionedKind::Compaction {
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
        } => Entry::Compaction(CompactionEntry {
            base,
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
        }),
        ProvisionedKind::BranchSummary {
            from_id,
            summary,
            details,
            usage,
        } => Entry::BranchSummary(BranchSummaryEntry {
            base,
            from_id,
            summary,
            details,
            usage,
        }),
        ProvisionedKind::Custom { custom_type, data } => Entry::Custom(CustomEntry {
            base,
            custom_type,
            data,
        }),
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Shared fields on every lane record. Mirrors TS `RecordBase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordBase {
    pub id: String,
    pub seq: u64,
    pub lane: String,
    pub timestamp: i64,
}

/// `OperationStartedRecord["intent"]` — the three durable operation intents.
///
/// Each variant renames itself to its TS `kind` value (`run`/`compaction`/
/// `navigation`) and renames its fields to camelCase to match the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OperationIntent {
    #[serde(rename = "run", rename_all = "camelCase")]
    Run {
        /// Normalized caller input before `before_run`; kept for suspended
        /// operations and `before_resume`.
        original_prompt: Vec<rpi_agent::message::AgentMessage>,
        /// Captured nextRun items, then the prompt, then before_run injections.
        initial_messages: Vec<ProvisionedEntryJSON>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt_override: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_data: Option<BTreeMap<String, JsonValue>>,
    },
    #[serde(rename = "compaction", rename_all = "camelCase")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        result_entry_id: String,
    },
    #[serde(rename = "navigation", rename_all = "camelCase")]
    Navigation {
        target_id: Option<String>,
        summarize: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_entry_id: Option<String>,
    },
}

impl OperationIntent {
    pub fn kind(&self) -> &'static str {
        match self {
            OperationIntent::Run { .. } => "run",
            OperationIntent::Compaction { .. } => "compaction",
            OperationIntent::Navigation { .. } => "navigation",
        }
    }
}

/// `ProvisionedEntry` as it appears inside an `operation_started` intent's
/// `initialMessages` or a `queue_enqueued`/`write_deferred` `target`. The TS
/// side carries `ProvisionedEntry` (which omits storage fields). On the wire we
/// serialize the same flat shape; in-process we keep the typed
/// [`ProvisionedEntry`] and round-trip through JSON at the codec boundary.
pub type ProvisionedEntryJSON = serde_json::Value;

/// `OperationStartedRecord`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStartedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub source_leaf_id: Option<String>,
    pub intent: OperationIntent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbortRequestedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    Completed,
    Aborted,
    Failed,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationFinishedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub outcome: OperationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationError>,
    /// Stable run result identity; absent on legacy/non-run terminal records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RunResultIdentity>,
}

/// References existing entries instead of duplicating assistant payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResultIdentity {
    pub leaf_id: String,
    pub final_entry_id: Option<String>,
    pub assistant_entry_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// Why compaction started — mirrors `CompactionReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Reified `StepAttemptRecord["step"]` for in-process dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Assistant,
    BranchSummary,
    Compaction,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Assistant => "assistant",
            StepKind::BranchSummary => "branch_summary",
            StepKind::Compaction => "compaction",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "assistant" => Some(StepKind::Assistant),
            "branch_summary" => Some(StepKind::BranchSummary),
            "compaction" => Some(StepKind::Compaction),
            _ => None,
        }
    }
}

/// `StepAttemptRecord`. TS unions two shapes (compaction step carries
/// `compactionReason`); the Rust port uses one struct with an optional field so
/// the `validateAttemptReason` reducer check can enforce the same invariant.
/// `step` is the on-wire discriminator and is NOT flattened, so it carries both
/// serde's tagged `step` field and a typed `step_kind` view (derived from
/// `step_raw`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepAttemptRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    /// On-wire `step` discriminator (`assistant` | `branch_summary` |
    /// `compaction`). Renamed via the field name `step`.
    pub step: StepKind,
    pub attempt: u32,
    pub result_entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_reason: Option<CompactionReason>,
}

impl StepAttemptRecord {
    pub fn step_kind(&self) -> StepKind {
        self.step
    }
}

/// One durable step of a streamed assistant message.
///
/// Frames are **progress, not history**: they live on the record stream rather
/// than the branch, so they can never appear in the branch path the model's
/// context is built from. Mirrors upstream's `pendingAssistantFrames` list
/// storage, adapted to rpi's append-only record log — see
/// `docs/debugging/llm-repetition-forensics.md` §十一.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantFrameRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    /// Which streamed message in the run this frame belongs to. A retried turn
    /// opens a new index, so a run can carry several streams.
    pub stream_index: usize,
    pub op: AssistantFrameOp,
    /// Present for [`AssistantFrameOp::Append`]: the encoded frame
    /// (`rpi_ai::frames::AssistantMessageFrame` as JSON). For `CommitStream`,
    /// carries `{ "entryId": ... }`, the entry reserved before committing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantFrameOp {
    /// One frame to append to the stream.
    Append,
    /// Bind a stream to a reserved entry before writing that entry. Recovery
    /// retires the stream only if the entry exists, covering crashes on either
    /// side of the commit without discarding uncommitted progress.
    CommitStream,
    /// Retire one stream: its message has been committed as an entry, so salvage
    /// must not replay it.
    ///
    /// Needed *mid-run*, because commit-on-settle commits an assistant message as
    /// soon as it settles and `ClearRun` only arrives when the whole run is done.
    /// Without this, a crash in a later turn made salvage replay the earlier
    /// turns too, duplicating them in the transcript.
    ClearStream,
    /// Retire every frame of the run: the run's messages are now persisted, so
    /// the frames were only progress.
    ClearRun,
}

/// `RetryPendingRecord` — a retryable attempt failed and another is scheduled.
///
/// Exists because the frames cannot say *why* a stream ended: a failed attempt
/// leaves `start`/content frames and no terminal frame, so `reduce_frames` yields
/// `stop_reason: pending` and "this was a retryable failure" is unreadable from
/// the frames. Nothing else about a failed attempt is persisted either (a
/// `step_attempt` is only written when a message commits).
///
/// So the retry intent gets its own record rather than reusing `step_attempt`:
/// that record's `attempt` field is validated as a *series* (a landed result ends
/// the series, so a multi-turn run's messages are independent series each at
/// attempt 1), which collides with numbering retry chains. A separate record
/// leaves the reducer's attempt rule untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPendingRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    /// The attempt that is about to start (1-based within the retry chain).
    pub attempt: u32,
    /// The entry id reserved for the attempt's assistant message. Recovery retries
    /// only while this never landed — once it exists, that attempt succeeded.
    pub result_entry_id: String,
}

/// A tool invocation coming into existence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStartedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub effective_args: serde_json::Value,
    pub result_entry_id: String,
    pub replay: ToolReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReplay {
    Never,
    Safe,
}

/// `QueueEnqueuedRecord["queue"]`. camelCase so `FollowUp` → `"followUp"` and
/// `NextRun` → `"nextRun"` (the TS spelling), NOT snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueKind {
    Steer,
    FollowUp,
    NextRun,
    /// Context-only messages appended during an operation. They are committed
    /// at a turn boundary and never start a new model turn by themselves.
    Context,
}

impl QueueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueKind::Steer => "steer",
            QueueKind::FollowUp => "followUp",
            QueueKind::NextRun => "nextRun",
            QueueKind::Context => "context",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEnqueuedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub queue: QueueKind,
    /// Legacy accepting-operation association. New queues belong to the lane
    /// and leave this unset, so acceptance can precede or outlive a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub target: ProvisionedEntryJSON,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueCancelledRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    /// Matches the original enqueue, including legacy operation associations;
    /// the cancellation may be recorded after that operation has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub entry_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteDeferredRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub target: ProvisionedEntryJSON,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCause {
    Assistant,
    Compaction,
    BranchSummary,
    DeferredFetch,
    Tool,
    Hook,
    Adjustment,
}

/// `UsageRecord`. TS unions a shared base with a `cause`-discriminated tail; we
/// flatten the base and keep the union's optional fields as `Option`s so the
/// reducer's cause-specific checks can run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub usage: Usage,
    pub cause: UsageCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Assistant/compaction/branch_summary/deferred-fetch cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<SessionStopReason>,
    /// Tool cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Adjustment cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
}

/// `LaneRecord` — the durable operation log. Tagged on `type` for serde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaneRecord {
    OperationStarted(OperationStartedRecord),
    AbortRequested(AbortRequestedRecord),
    OperationFinished(OperationFinishedRecord),
    StepAttempt(StepAttemptRecord),
    ToolStarted(ToolStartedRecord),
    QueueEnqueued(QueueEnqueuedRecord),
    QueueCancelled(QueueCancelledRecord),
    WriteDeferred(WriteDeferredRecord),
    Usage(UsageRecord),
    /// Streamed assistant-message progress. On the record stream, **not** the
    /// branch: frames must never enter the context the model is built from.
    AssistantFrame(AssistantFrameRecord),
    /// Scheduled retry: an attempt failed retryably and another is about to start.
    RetryPending(RetryPendingRecord),
}

impl LaneRecord {
    pub fn base(&self) -> &RecordBase {
        match self {
            LaneRecord::OperationStarted(r) => &r.base,
            LaneRecord::AbortRequested(r) => &r.base,
            LaneRecord::OperationFinished(r) => &r.base,
            LaneRecord::StepAttempt(r) => &r.base,
            LaneRecord::ToolStarted(r) => &r.base,
            LaneRecord::QueueEnqueued(r) => &r.base,
            LaneRecord::QueueCancelled(r) => &r.base,
            LaneRecord::WriteDeferred(r) => &r.base,
            LaneRecord::Usage(r) => &r.base,
            LaneRecord::AssistantFrame(r) => &r.base,
            LaneRecord::RetryPending(r) => &r.base,
        }
    }
    pub fn seq(&self) -> u64 {
        self.base().seq
    }
    pub fn lane(&self) -> &str {
        &self.base().lane
    }
    pub fn id(&self) -> &str {
        &self.base().id
    }
    pub fn record_type(&self) -> &'static str {
        match self {
            LaneRecord::OperationStarted(_) => "operation_started",
            LaneRecord::AbortRequested(_) => "abort_requested",
            LaneRecord::OperationFinished(_) => "operation_finished",
            LaneRecord::StepAttempt(_) => "step_attempt",
            LaneRecord::ToolStarted(_) => "tool_started",
            LaneRecord::QueueEnqueued(_) => "queue_enqueued",
            LaneRecord::QueueCancelled(_) => "queue_cancelled",
            LaneRecord::WriteDeferred(_) => "write_deferred",
            LaneRecord::Usage(_) => "usage",
            LaneRecord::AssistantFrame(_) => "assistant_frame",
            LaneRecord::RetryPending(_) => "retry_pending",
        }
    }
    /// `runId` association (mirrors TS `hasRunId`), including the provenance of
    /// legacy queues. A queue cancellation can outlive its accepting operation.
    /// The operation_started record's *identity* is its `id`, handled separately.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            LaneRecord::OperationStarted(_) => None,
            LaneRecord::AbortRequested(r) => Some(&r.run_id),
            LaneRecord::OperationFinished(r) => Some(&r.run_id),
            LaneRecord::StepAttempt(r) => Some(&r.run_id),
            LaneRecord::ToolStarted(r) => Some(&r.run_id),
            LaneRecord::QueueEnqueued(r) => r.run_id.as_deref(),
            LaneRecord::QueueCancelled(r) => r.run_id.as_deref(),
            LaneRecord::WriteDeferred(r) => Some(&r.run_id),
            LaneRecord::Usage(r) => r.run_id.as_deref(),
            LaneRecord::AssistantFrame(r) => Some(&r.run_id),
            LaneRecord::RetryPending(r) => Some(&r.run_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Queries + metadata + stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryOrder {
    #[default]
    NewestFirst,
    OldestFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryCursor {
    pub after_seq: u64,
}

/// Reserved native archival data, not context-transform/projector input.
/// Match exact versioned types; ordinary extension custom entries stay visible.
pub(crate) fn is_tool_program_body_type(custom_type: &str) -> bool {
    matches!(
        custom_type,
        crate::tool_program_body::CHUNK_CUSTOM_TYPE | crate::tool_program_body::COMMIT_CUSTOM_TYPE
    )
}

#[derive(Debug, Clone, Default)]
pub struct EntryQuery {
    /// Internal context/recovery policy, applied before payload hydration.
    /// False by default: explicit history/export queries still expose bodies.
    pub exclude_tool_program_bodies: bool,
    pub entry_type: Option<&'static str>,
    pub custom_type: Option<String>,
    /// Exact identity, useful for a bounded branch-membership check.
    pub entry_id: Option<String>,
    /// Tool inspection filters before cloning unrelated transcript payloads.
    pub tool_call_id: Option<String>,
    /// Select assistant/user boundaries for bounded legacy tool correlation.
    pub message_boundary_only: bool,
    /// Host-managed custom-message outbox metadata. Filtering precedes cloning.
    pub outbox_key: Option<String>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
    pub cursor: Option<EntryCursor>,
}

/// Real entry identity and parent link, without a transcript payload.
/// Ancestry includes all entry kinds, including archival custom entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryAncestry {
    pub id: String,
    pub parent_id: Option<String>,
}

/// Branch-scan bounds. Mirrors TS `BranchBounds`.
#[derive(Debug, Clone, Default)]
pub struct BranchBounds {
    /// Default: the view's lane leaf.
    pub start: Option<String>,
    /// Scan ends after the first match, inclusive.
    pub stop_at_type: Option<&'static str>,
    pub stop_at_id: Option<String>,
}

// Bounded transcript reads. These are session/storage types, not UI or RPC types.
/// Chronological transcript window ending exclusively before `before_id`.
#[derive(Debug, Clone)]
pub struct TranscriptPageQuery {
    pub limit: usize,
    pub before_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub entries: Vec<Entry>,
    pub total: usize,
    pub has_more: bool,
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageSummaryQuery {
    #[serde(default = "default_summary_limit")]
    pub limit: usize,
    #[serde(default = "default_summary_role")]
    pub role: String,
    pub before_id: Option<String>,
    pub leaf_id: Option<String>,
}
fn default_summary_limit() -> usize {
    10
}
fn default_summary_role() -> String {
    "user".into()
}
impl Default for MessageSummaryQuery {
    fn default() -> Self {
        Self {
            limit: 10,
            role: default_summary_role(),
            before_id: None,
            leaf_id: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub entry_id: String,
    pub role: String,
    pub preview: String,
    pub timestamp: i64,
    pub has_images: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummaryPage {
    pub filtered: bool,
    pub role: String,
    pub messages: Vec<MessageSummary>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub next_before_id: Option<String>,
    pub has_more: bool,
    pub leaf_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageWindowQuery {
    #[serde(default = "default_window_limit")]
    pub limit: usize,
    pub anchor_id: Option<String>,
    pub before_id: Option<String>,
    pub after_id: Option<String>,
    pub leaf_id: Option<String>,
}
fn default_window_limit() -> usize {
    48
}
impl Default for MessageWindowQuery {
    fn default() -> Self {
        Self {
            limit: 48,
            anchor_id: None,
            before_id: None,
            after_id: None,
            leaf_id: None,
        }
    }
}
#[derive(Debug, Clone)]
pub struct MessageWindowPage {
    pub entries: Vec<Entry>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub next_before_id: Option<String>,
    pub next_after_id: Option<String>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    pub leaf_id: Option<String>,
}

/// A bounded, UI-independent page of every entry variant on one pinned branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BranchEntryPageQuery {
    pub leaf_id: Option<String>,
    pub before_id: Option<String>,
    pub after_id: Option<String>,
    #[serde(default = "default_branch_page_limit")]
    pub limit: usize,
    #[serde(default = "default_branch_page_bytes")]
    pub max_bytes: usize,
}
fn default_branch_page_limit() -> usize {
    50
}
fn default_branch_page_bytes() -> usize {
    4 * 1024 * 1024
}
impl Default for BranchEntryPageQuery {
    fn default() -> Self {
        Self {
            leaf_id: None,
            before_id: None,
            after_id: None,
            limit: default_branch_page_limit(),
            max_bytes: default_branch_page_bytes(),
        }
    }
}

/// Entries are chronological. Cursors are exclusive and belong to `leaf_id`.
/// The byte budget counts complete JSON entries (not envelope/separators); an
/// oversized entry is returned alone, never truncated or silently skipped.
#[derive(Debug, Clone)]
pub struct BranchEntryPage {
    pub entries: Vec<Entry>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub next_before_id: Option<String>,
    pub next_after_id: Option<String>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    pub leaf_id: Option<String>,
}

/// Full-branch settings and assistant-only usage, without context messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchOverview {
    pub thinking_level: String,
    pub model: Option<(String, String)>,
    pub active_tool_names: Option<Vec<String>>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_total: f64,
    /// Latest eligible assistant cache-hit percentage, not an aggregate rate.
    pub cache_hit_rate: Option<f64>,
    /// Latest positive successful measurement since the last compaction.
    pub context_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct RecordQuery {
    pub lane: Option<String>,
    pub record_type: Option<&'static str>,
    pub run_id: Option<String>,
    /// Exact operation intent kind. Valid only with type `operation_started`.
    pub operation_kind: Option<&'static str>,
    /// Queue filters are applied before copying payloads or applying the limit.
    pub queue_kind: Option<QueueKind>,
    pub tool_call_id: Option<String>,
    pub target_entry_id: Option<String>,
    /// Select only enqueues with neither a committed target nor a cancellation.
    pub pending_queue_only: bool,
    /// Exclusive chronological lower bound: `seq > after_seq`, regardless of order.
    pub after_seq: Option<u64>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
}

/// A bounded chronological page. Bounds are exclusive; `from_end` selects
/// the newest matching window without changing the ascending output order.
#[derive(Debug, Clone)]
pub struct RecordPageQuery {
    pub lane: String,
    pub after_seq: Option<u64>,
    pub before_seq: Option<u64>,
    pub limit: usize,
    pub from_end: bool,
}

#[derive(Debug, Clone)]
pub struct RecordPage {
    pub records: Vec<LaneRecord>,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: i64,
    pub uncached_tokens: i64,
    pub total_tokens: i64,
    pub cost_total: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanePointer {
    pub lane: String,
    pub leaf_id: Option<String>,
}

/// Authoritative live lane identity. `generation` advances only for explicit
/// lane create/move mutations; ordinary entry/record appends do not change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLaneBranch {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub generation: u64,
}

/// `LogItem` — the unified mutation log `get_log` returns. Mirrors the TS union
/// (`entry | record | lane | fact(name) | fact(label)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogItem {
    Entry {
        seq: u64,
        entry: Entry,
    },
    Record {
        seq: u64,
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    FactName {
        seq: u64,
        name: Option<String>,
    },
    FactLabel {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

impl LogItem {
    pub fn seq(&self) -> u64 {
        match self {
            LogItem::Entry { seq, .. }
            | LogItem::Record { seq, .. }
            | LogItem::Lane { seq, .. }
            | LogItem::FactName { seq, .. }
            | LogItem::FactLabel { seq, .. } => *seq,
        }
    }
}

/// Mirrors TS `SessionMutation` (the in-process mutation log the state reduces).
///
/// Note the `entry` arm carries a **full** [`Entry`] (storage fields
/// `seq`/`parent_id`/`timestamp` already stamped by the storage layer), matching
/// the TS `SessionMutation` shape exactly — the state *validates* lane-leaf
/// chaining against `entry.parent_id`, it does not set it. The fork path
/// preserves each entry's original `parent_id` chain and passes `lane: None` so
/// chaining is not re-checked (invariant §7).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionMutation {
    /// Append an entry to `lane` (fork path passes `lane: None` — chaining is
    /// checked only when `Some`, per invariant §7).
    Entry {
        seq: u64,
        timestamp: i64,
        lane: Option<String>,
        entry: Entry,
    },
    /// Append a lane record (seq/timestamp already stamped by the caller).
    Record { record: LaneRecord },
    /// Move/create a lane leaf.
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    /// Global name fact (latest-wins).
    FactName { seq: u64, name: Option<String> },
    /// Global label fact (latest-wins per target).
    FactLabel {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

impl SessionMutation {
    pub fn seq(&self) -> u64 {
        match self {
            SessionMutation::Entry { seq, .. }
            | SessionMutation::Lane { seq, .. }
            | SessionMutation::FactName { seq, .. }
            | SessionMutation::FactLabel { seq, .. } => *seq,
            SessionMutation::Record { record } => record.seq(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// Fork scope — mirrors TS `ForkOptions`.
#[derive(Debug, Clone)]
pub enum ForkOptions {
    /// Fork the whole tree (all lanes, all entries).
    Tree,
    /// Fork a single branch up to `entry_id` (default: main lane leaf).
    Branch {
        entry_id: Option<String>,
        position: Option<ForkPosition>,
    },
}

impl Default for ForkOptions {
    /// TS default is branch-scope at the lane leaf.
    fn default() -> Self {
        ForkOptions::Branch {
            entry_id: None,
            position: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}

impl Default for ForkPosition {
    fn default() -> Self {
        ForkPosition::At
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
    /// Opaque application-owned metadata. The in-memory repo ignores it; the
    /// JSONL repo persists it into the v4 header and surfaces it on
    /// [`crate::session::jsonl::types::JsonlSessionMetadata`].
    pub metadata: Option<serde_json::Map<String, JsonValue>>,
}

// ---------------------------------------------------------------------------
// SessionStorage / SessionRepo / SessionTree traits
// ---------------------------------------------------------------------------

/// Read+write surface the `Session` wraps. Mirrors TS `SessionStorage`.
///
/// All methods are `async` because the JSONL backend hits the FS; the in-memory
/// backend is synchronous under the hood but still `async` for trait uniformity.
/// Reads return **clones** (defensive-copy contract); writes return the stamped
/// entry/record clone.
///
/// Adaptation note: TS `appendRecord` takes a `NewRecord` (record minus
/// `seq`/`timestamp`). The Rust port has the caller (the state/Session layer)
/// stamp `seq`+`timestamp` before delegating, so `append_record` takes a fully
/// formed [`LaneRecord`] and persists it as-is, returning a clone. This keeps a
/// single `LaneRecord` type instead of a parallel `NewLaneRecord` enum.
#[async_trait::async_trait]
pub trait SessionStorage: Send + Sync {
    fn metadata(&self) -> SessionMetadata;
    async fn get_metadata(&self) -> SessionMetadata {
        self.metadata()
    }

    /// Whether this storage never persists session data beyond the process.
    /// Memory backends return `true`; durable backends (JSONL, SQLite) keep
    /// the `false` default. Hosts consult this so an explicit no-persistence
    /// choice survives session churn — e.g. a `/new` in ephemeral mode must
    /// swap in another in-memory session, not silently write a file.
    fn is_ephemeral(&self) -> bool {
        false
    }

    // Lanes
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>>;
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()>;
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()>;

    /// Atomically read one lane's current leaf and explicit lane-mutation
    /// generation. Custom storage defaults to fail-closed: callers must not
    /// substitute the ordinary leaf or history revision for this identity.
    async fn lane_branch(&self, lane: &str) -> SessionResult<SessionLaneBranch> {
        let _ = lane;
        Err(SessionError::storage("lane branch identity is unsupported"))
    }

    // Transient payloads use a separate sequence domain and never enter history.
    // CommitStream bindings remain immutable lane records.
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>>;
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>>;
    /// Retire payloads proven terminal by this session's durable OperationFinished.
    /// Returns deleted payload count; neither history nor progress sequence advances.
    async fn prune_completed_progress(&self) -> SessionResult<usize>;

    // Entries + records
    /// Snapshot token for optimistic, conditional history writes. Transient
    /// progress must not advance it. Unsupported backends fail closed.
    async fn history_revision(&self) -> SessionResult<u64> {
        Err(SessionError::storage(
            "conditional history writes are unsupported",
        ))
    }

    /// Compare the original revision and append within the storage's mutation
    /// boundary. Never implement this as a read followed by ordinary append.
    async fn append_entry_if_revision(
        &self,
        _entry: ProvisionedEntry,
        _lane: &str,
        _expected: u64,
    ) -> SessionResult<Entry> {
        Err(SessionError::storage(
            "conditional history writes are unsupported",
        ))
    }

    async fn append_record_if_revision(
        &self,
        _record: LaneRecord,
        _expected: u64,
    ) -> SessionResult<LaneRecord> {
        Err(SessionError::storage(
            "conditional history writes are unsupported",
        ))
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry>;
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord>;

    /// Accept a cancellation batch without exposing a partially reduced batch.
    /// This is an ACK boundary, not a power-loss transaction guarantee.
    async fn append_queue_cancellations(
        &self,
        _records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        Err(SessionError::storage(
            "queue cancellation batches are unsupported",
        ))
    }

    async fn append_record_and_entry(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        let record = self.append_record(record).await?;
        let entry = self.append_entry(entry, lane).await?;
        Ok((record, entry))
    }

    /// Append a record, an entry, and a following record in that exact logical
    /// order. Durable backends may encode all three mutations together.
    async fn append_record_entry_and_record(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        let before = self.append_record(before).await?;
        let entry = self.append_entry(entry, lane).await?;
        let after = self.append_record(after).await?;
        Ok((before, entry, after))
    }

    /// Append several records as one logical batch. Backends that do not have
    /// a native batch writer retain the existing semantics by falling back to
    /// ordered single-record appends.
    async fn append_records(&self, records: Vec<LaneRecord>) -> SessionResult<Vec<LaneRecord>> {
        let mut stamped = Vec::with_capacity(records.len());
        for record in records {
            stamped.push(self.append_record(record).await?);
        }
        Ok(stamped)
    }

    // Reads
    /// Optional bounded estimate for the default, hook-free lane context.
    /// `None` means unsupported; read/validation failures must remain errors.
    async fn context_usage(
        &self,
        _lane: &str,
    ) -> SessionResult<Option<crate::compaction::ContextUsageEstimate>> {
        Ok(None)
    }

    /// Optional metadata-first transcript paging. `None` means unsupported;
    /// supported backends return errors rather than falling back on failure.
    async fn transcript_page(
        &self,
        _lane: &str,
        _query: &TranscriptPageQuery,
    ) -> SessionResult<Option<TranscriptPage>> {
        Ok(None)
    }
    async fn branch_entry_page(
        &self,
        lane: &str,
        query: &BranchEntryPageQuery,
    ) -> SessionResult<BranchEntryPage> {
        crate::session::branch_queries::storage_branch_entry_page(self, lane, query).await
    }
    async fn branch_overview(&self, lane: &str) -> SessionResult<BranchOverview> {
        crate::session::branch_queries::storage_branch_overview(self, lane).await
    }
    async fn message_summaries(
        &self,
        lane: &str,
        query: &MessageSummaryQuery,
    ) -> SessionResult<MessageSummaryPage> {
        let _ = (lane, query);
        Err(crate::error::SessionError::new(
            crate::error::SessionErrorCode::InvalidQuery,
            "message summaries unsupported by storage",
        ))
    }
    async fn message_window(
        &self,
        lane: &str,
        query: &MessageWindowQuery,
    ) -> SessionResult<MessageWindowPage> {
        let _ = (lane, query);
        Err(crate::error::SessionError::new(
            crate::error::SessionErrorCode::InvalidQuery,
            "message windows unsupported by storage",
        ))
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>>;
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>>;
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>>;

    /// Validated ancestry, newest first, inclusive of the first matching bound.
    /// `start` is the resolved branch tip; `bounds.start` is resolved by the view.
    /// Native metadata-backed stores avoid payload reads. The compatibility
    /// fallback hydrates full entries through the ordinary, unfiltered query;
    /// custom stores should override this method to provide metadata-only reads.
    async fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<EntryAncestry>> {
        Ok(self
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::NewestFirst),
                    ..Default::default()
                },
                bounds,
                start,
            )
            .await?
            .into_iter()
            .map(|entry| EntryAncestry {
                id: entry.id().to_owned(),
                parent_id: entry.parent_id().map(str::to_owned),
            })
            .collect())
    }

    /// Default, hook-free context projection. Backends may override this to
    /// walk their owned state while borrowing entries; the fallback preserves
    /// custom-storage compatibility and defensive-copy semantics.
    async fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        let path = self
            .find_entries_on_branch(
                &EntryQuery {
                    exclude_tool_program_bodies: true,
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                bounds,
                start,
            )
            .await?;
        Ok(crate::session::context::build_session_context_owned(
            path,
            &Default::default(),
        ))
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>>;
    /// Count and select within one storage snapshot, cloning only the page.
    async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage>;
    /// Returns unfinished `operation_started` records, newest first. Recovery
    /// uses `limit: 2`: 0 = idle, 1 = suspended, 2 = corruption.
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>>;
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>>;

    // Global facts (latest-wins, not branch-scoped)
    async fn get_name(&self) -> SessionResult<Option<String>>;
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()>;
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>>;
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()>;
    async fn get_stats(&self) -> SessionResult<SessionStats>;

    /// Wait for any in-flight durable write to settle.
    ///
    /// Only durable backends have anything to wait for. The in-memory backend is
    /// consistent by construction and the SQLite backend commits each mutation
    /// inside the call that returns it, so the default is a no-op.
    async fn drain(&self) {}
}

/// Repo-level lifecycle. Mirrors TS `SessionRepo`.
#[async_trait::async_trait]
pub trait SessionRepo: Send + Sync {
    type Storage: SessionStorage;
    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage>;
    /// Opens the session for writing and acquires any backend writer claim.
    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage>;
    async fn list(&self) -> SessionResult<Vec<SessionMetadata>>;
    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()>;
    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage>;
}

/// Branch-scoped read+write view the harness exposes per lane. Mirrors TS
/// `SessionTree`. Lane-bound harness runners own one of these views.
#[async_trait::async_trait]
pub trait SessionTree: Send + Sync {
    /// Optional bounded estimate for this lane's default, hook-free context.
    /// `None` means unsupported, not an error fallback.
    async fn context_usage(
        &self,
    ) -> SessionResult<Option<crate::compaction::ContextUsageEstimate>> {
        Ok(None)
    }

    /// Optional metadata-first transcript paging for this lane-bound view.
    async fn transcript_page(
        &self,
        _query: &TranscriptPageQuery,
    ) -> SessionResult<Option<TranscriptPage>> {
        Ok(None)
    }
    async fn branch_entry_page(
        &self,
        query: &BranchEntryPageQuery,
    ) -> SessionResult<BranchEntryPage> {
        crate::session::branch_queries::tree_branch_entry_page(self, query).await
    }
    async fn branch_overview(&self) -> SessionResult<BranchOverview> {
        crate::session::branch_queries::tree_branch_overview(self).await
    }
    async fn message_summaries(
        &self,
        query: &MessageSummaryQuery,
    ) -> SessionResult<MessageSummaryPage> {
        let _ = query;
        Err(crate::error::SessionError::new(
            crate::error::SessionErrorCode::InvalidQuery,
            "message summaries unsupported by tree",
        ))
    }
    async fn message_window(&self, query: &MessageWindowQuery) -> SessionResult<MessageWindowPage> {
        let _ = query;
        Err(crate::error::SessionError::new(
            crate::error::SessionErrorCode::InvalidQuery,
            "message windows unsupported by tree",
        ))
    }
    async fn get_leaf_id(&self) -> SessionResult<Option<String>>;
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>>;
    async fn get_stats(&self) -> SessionResult<SessionStats>;

    async fn get_name(&self) -> SessionResult<Option<String>>;
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()>;
    async fn get_label(&self, target_id: &str) -> SessionResult<Option<String>>;
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> SessionResult<()>;

    /// Session-wide, all branches, sequence order.
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>>;
    async fn find_entry(&self, query: &EntryQuery) -> SessionResult<Option<Entry>>;
    /// Branch-scoped: the path from `bounds.start` (default: lane leaf) toward root.
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>>;
    /// All identities on this branch, newest first, inclusive of the first
    /// matching bound. Native views forward to storage metadata without loading
    /// bodies. This compatibility fallback may hydrate full entries on custom
    /// trees; override it to avoid those reads. Empty lane yields an empty path.
    async fn branch_ancestry(&self, bounds: &BranchBounds) -> SessionResult<Vec<EntryAncestry>> {
        Ok(self
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::NewestFirst),
                    ..Default::default()
                },
                bounds,
            )
            .await?
            .into_iter()
            .map(|entry| EntryAncestry {
                id: entry.id().to_owned(),
                parent_id: entry.parent_id().map(str::to_owned),
            })
            .collect())
    }

    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Option<Entry>>;

    /// Writes resolve on durable acceptance; the returned id is the entry's id
    /// (provisioned when the write defers).
    async fn append_message(
        &self,
        message: rpi_agent::message::AgentMessage,
    ) -> SessionResult<String>;
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> SessionResult<String>;
}

/// Used by `SuspendedOperation` snapshots and `AbortResult`. Carries a deferred
/// provider handle returned from a long-poll stream.
pub type SuspendedDeferred = DeferredHandle;
