//! Mirrors `packages/agent/src/harness/result.ts` (`TaggedError` / `matchError`)
//! and the `AgentHarness` rejection classes in `agent-harness.ts`.
//!
//! TS models rejections as `TaggedError` factories — each creates an `Error`
//! subclass carrying a `_tag` plus a props bag, dispatched via `matchError`.
//! Rust replaces this with one `HarnessError` enum (each variant IS a tag,
//! carrying its props as fields) plus a [`HarnessError::tag`] accessor. Plain
//! `match` replaces `matchError` — no dynamic dispatch, no `toJSON` plumbing.
//!
//! Expected (recoverable) operation rejections are `HarnessError`; unexpected
//! infrastructure faults are the separate [`HarnessFault`] / [`HarnessClosed`]
//! / [`HarnessNotImplemented`] types, mirroring the TS distinction.

use thiserror::Error;

/// Stable tag strings, matching the TS `TaggedError` `_tag` values exactly so the
/// wire shape (and any future `toJSON`) stays compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessErrorTag {
    LaneBusy,
    MissingIdentities,
    NoActiveRun,
    NoActiveOperation,
    NothingToResume,
    InvalidMessage,
    UnknownSkill,
    UnknownTemplate,
    UnknownTarget,
    UnknownQueueItem,
    LaneExists,
    InvalidLane,
    NothingToCompact,
    Closed,
    Io,
    Agent,
    Compaction,
    /// A durable session write was refused. Distinct from `Io` so a host can tell
    /// "the disk would not take the write" from "the provider request failed":
    /// the user's next action differs (`/export` vs retry).
    SessionStorage,
    /// The requested capability is recognized but not implemented in this
    /// port. Carries the deferred feature name for diagnostics.
    NotImplemented,
}

impl HarnessErrorTag {
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessErrorTag::LaneBusy => "LaneBusy",
            HarnessErrorTag::MissingIdentities => "MissingIdentities",
            HarnessErrorTag::NoActiveRun => "NoActiveRun",
            HarnessErrorTag::NoActiveOperation => "NoActiveOperation",
            HarnessErrorTag::NothingToResume => "NothingToResume",
            HarnessErrorTag::InvalidMessage => "InvalidMessage",
            HarnessErrorTag::UnknownSkill => "UnknownSkill",
            HarnessErrorTag::UnknownTemplate => "UnknownTemplate",
            HarnessErrorTag::UnknownTarget => "UnknownTarget",
            HarnessErrorTag::UnknownQueueItem => "UnknownQueueItem",
            HarnessErrorTag::LaneExists => "LaneExists",
            HarnessErrorTag::InvalidLane => "InvalidLane",
            HarnessErrorTag::NothingToCompact => "NothingToCompact",
            HarnessErrorTag::Closed => "Closed",
            HarnessErrorTag::Io => "Io",
            HarnessErrorTag::Agent => "Agent",
            HarnessErrorTag::Compaction => "Compaction",
            HarnessErrorTag::SessionStorage => "SessionStorage",
            HarnessErrorTag::NotImplemented => "NotImplemented",
        }
    }
}

/// `OperationKind` — the three durable operation intents. Shared by `LaneBusy`
/// and lane snapshots. Mirrors TS `"run" | "compaction" | "navigation"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Run,
    Compaction,
    Navigation,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OperationKind::Run => "run",
            OperationKind::Compaction => "compaction",
            OperationKind::Navigation => "navigation",
        }
    }
}

/// One tagged rejection from the harness. The variants are the TS
/// `TaggedError("…")` subclasses from `agent-harness.ts`; `tag()` replaces the
/// `_tag` field and `match` replaces `matchError`.
///
/// Every variant carries the fields its TS counterpart did (minus `message`,
/// which `thiserror`'s `#[error]` supplies via the `message` accessor below).
#[derive(Debug, Clone, Error)]
pub enum HarnessError {
    #[error("{message}")]
    LaneBusy {
        lane: String,
        operation_id: String,
        operation_kind: OperationKind,
        message: String,
    },
    #[error("{message}")]
    MissingIdentities {
        lane: String,
        tools: Vec<String>,
        models: Vec<String>,
        message: String,
    },
    #[error("{message}")]
    NoActiveRun { lane: String, message: String },
    #[error("{message}")]
    NoActiveOperation { lane: String, message: String },
    #[error("{message}")]
    NothingToResume { lane: String, message: String },
    #[error("{message}")]
    InvalidMessage {
        lane: String,
        reason: String,
        message: String,
    },
    #[error("{message}")]
    UnknownSkill { name: String, message: String },
    #[error("{message}")]
    UnknownTemplate { name: String, message: String },
    #[error("{message}")]
    UnknownTarget { target_id: String, message: String },
    #[error("{message}")]
    UnknownQueueItem {
        lane: String,
        entry_id: String,
        message: String,
    },
    #[error("{message}")]
    LaneExists { lane: String, message: String },
    #[error("{message}")]
    InvalidLane {
        lane: String,
        reason: String,
        message: String,
    },
    #[error("{message}")]
    NothingToCompact { lane: String, message: String },
    #[error("{message}")]
    Closed { message: String },
    #[error("{message}")]
    Io { message: String },
    #[error("{message}")]
    Agent { message: String },
    #[error("{message}")]
    Compaction { message: String },
    /// A durable session write the backend refused. Distinct from `Io` so a host
    /// can tell "the disk would not take the write" from "the provider request
    /// failed": the user's next action differs (`/export` vs retry).
    #[error("{message}")]
    SessionStorage { message: String },
    /// The requested capability is recognized but not implemented yet. Keeps
    /// the deferred surface explicit instead of returning a silent `Ok`.
    #[error("{message}")]
    NotImplemented { feature: String, message: String },
}

impl HarnessError {
    /// Stable tag — the TS `_tag`. Use this to dispatch without matching every
    /// variant (the `matchError` replacement for cases that only need the tag).
    pub fn tag(&self) -> HarnessErrorTag {
        match self {
            HarnessError::LaneBusy { .. } => HarnessErrorTag::LaneBusy,
            HarnessError::MissingIdentities { .. } => HarnessErrorTag::MissingIdentities,
            HarnessError::NoActiveRun { .. } => HarnessErrorTag::NoActiveRun,
            HarnessError::NoActiveOperation { .. } => HarnessErrorTag::NoActiveOperation,
            HarnessError::NothingToResume { .. } => HarnessErrorTag::NothingToResume,
            HarnessError::InvalidMessage { .. } => HarnessErrorTag::InvalidMessage,
            HarnessError::UnknownSkill { .. } => HarnessErrorTag::UnknownSkill,
            HarnessError::UnknownTemplate { .. } => HarnessErrorTag::UnknownTemplate,
            HarnessError::UnknownTarget { .. } => HarnessErrorTag::UnknownTarget,
            HarnessError::UnknownQueueItem { .. } => HarnessErrorTag::UnknownQueueItem,
            HarnessError::LaneExists { .. } => HarnessErrorTag::LaneExists,
            HarnessError::InvalidLane { .. } => HarnessErrorTag::InvalidLane,
            HarnessError::NothingToCompact { .. } => HarnessErrorTag::NothingToCompact,
            HarnessError::Closed { .. } => HarnessErrorTag::Closed,
            HarnessError::Io { .. } => HarnessErrorTag::Io,
            HarnessError::Agent { .. } => HarnessErrorTag::Agent,
            HarnessError::Compaction { .. } => HarnessErrorTag::Compaction,
            HarnessError::SessionStorage { .. } => HarnessErrorTag::SessionStorage,
            HarnessError::NotImplemented { .. } => HarnessErrorTag::NotImplemented,
        }
    }

    /// True when the harness was closed while the operation was active. Mirrors
    /// the `Closed` rejection branch.
    pub fn is_closed(&self) -> bool {
        matches!(self, HarnessError::Closed { .. })
    }

    /// Convenience constructors matching the TS `new XxxTaggedError({...})` shape.
    pub fn lane_busy(
        lane: impl Into<String>,
        operation_id: impl Into<String>,
        operation_kind: OperationKind,
        message: impl Into<String>,
    ) -> Self {
        HarnessError::LaneBusy {
            lane: lane.into(),
            operation_id: operation_id.into(),
            operation_kind,
            message: message.into(),
        }
    }
    pub fn no_active_run(lane: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::NoActiveRun {
            lane: lane.into(),
            message: message.into(),
        }
    }
    pub fn no_active_operation(lane: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::NoActiveOperation {
            lane: lane.into(),
            message: message.into(),
        }
    }
    pub fn closed() -> Self {
        HarnessError::Closed {
            message: "AgentHarness was closed".into(),
        }
    }
    pub fn lane_exists(lane: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::LaneExists {
            lane: lane.into(),
            message: message.into(),
        }
    }
    pub fn invalid_lane(
        lane: impl Into<String>,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        HarnessError::InvalidLane {
            lane: lane.into(),
            reason: reason.into(),
            message: message.into(),
        }
    }
    pub fn unknown_skill(name: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::UnknownSkill {
            name: name.into(),
            message: message.into(),
        }
    }
    pub fn unknown_template(name: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::UnknownTemplate {
            name: name.into(),
            message: message.into(),
        }
    }
    pub fn unknown_target(target_id: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::UnknownTarget {
            target_id: target_id.into(),
            message: message.into(),
        }
    }
    pub fn nothing_to_compact(lane: impl Into<String>) -> Self {
        HarnessError::NothingToCompact {
            lane: lane.into(),
            message: "Nothing to compact".into(),
        }
    }
    pub fn agent(message: impl Into<String>) -> Self {
        HarnessError::Agent {
            message: message.into(),
        }
    }
    pub fn compaction(message: impl Into<String>) -> Self {
        HarnessError::Compaction {
            message: message.into(),
        }
    }
    pub fn io(message: impl Into<String>) -> Self {
        HarnessError::Io {
            message: message.into(),
        }
    }
    /// A durable session write the backend refused.
    ///
    /// Separate from [`Self::io`] so the host can act differently: an `Io` failure
    /// is usually a transient tool or filesystem problem, while this one means the
    /// conversation stopped being saved and `/export` is the user's only recovery.
    pub fn session_storage(message: impl Into<String>) -> Self {
        HarnessError::SessionStorage {
            message: message.into(),
        }
    }
    /// A recognized-but-deferred capability. `feature` is the API name; the
    /// message is generated so callers get a consistent diagnostic.
    pub fn not_implemented(feature: impl Into<String>) -> Self {
        let feature = feature.into();
        HarnessError::NotImplemented {
            message: format!("harness feature '{feature}' is not implemented"),
            feature,
        }
    }
}

/// Unexpected infrastructure fault (mirrors TS `HarnessFault`). Carries a cause
/// for diagnostics. Distinct from [`HarnessError`] (expected rejections) so the
/// recoverable-path `Result<_, HarnessError>` stays narrow.
#[derive(Debug, Clone, Error)]
#[error("HarnessFault: {message}")]
pub struct HarnessFault {
    pub message: String,
    pub cause: Option<String>,
}

impl HarnessFault {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
        }
    }
    pub fn with_cause(message: impl Into<String>, cause: impl ToString) -> Self {
        Self {
            message: message.into(),
            cause: Some(cause.to_string()),
        }
    }
}

/// Harness was closed mid-operation (mirrors TS `HarnessClosed`). A thin
/// `HarnessError::Closed` wrapper the loop raises when it observes `closed`
/// during an in-flight operation.
#[derive(Debug, Clone, Error)]
#[error("AgentHarness was closed while the operation was active")]
pub struct HarnessClosed;

impl From<HarnessClosed> for HarnessError {
    fn from(_: HarnessClosed) -> Self {
        HarnessError::closed()
    }
}

/// `Result<T, HarnessError>` — the standard harness-rejection return shape.
pub type HarnessResult<T> = std::result::Result<T, HarnessError>;
