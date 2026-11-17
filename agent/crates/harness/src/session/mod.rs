//! `session` submodule — mirrors `packages/agent/src/harness/session/*`.
//!
//! Durable session model ([`types`]), the in-memory state reducer ([`state`])
//! + record-log validator ([`reducer`]), the in-memory backend ([`memory`]),
//! the JSONL codec + storage + repo ([`jsonl`]), the context builder
//! ([`context`]) that projects a branch path to provider-facing messages, and
//! the `Session` + `SessionTree` facade ([`session`]) built over a
//! [`types::SessionStorage`].
//!
//! Built up incrementally across M5b–M5f.

#[cfg(test)]
mod storage_contract_tests;

pub(crate) mod branch_queries;
#[cfg(test)]
mod branch_queries_compat_tests;
#[cfg(test)]
mod branch_queries_tests;
pub mod context;
pub(crate) mod diagnostics;
#[cfg(feature = "experimental-native-store")]
pub mod experimental_native;
pub mod jsonl;
pub mod memory;
pub(crate) mod message_preview;
pub(crate) mod progress;
pub mod reducer;
pub mod session;
pub mod sqlite;
pub mod state;
pub mod types;
pub mod values;

pub use context::{
    build_context_entries, build_session_context, default_context_entry_transform,
    session_entry_to_context_messages, ContextEntryTransform, CustomEntryContextMessageProjector,
    SessionContext, SessionContextBuildOptions,
};
pub use memory::{
    Clock, CounterIdGenerator, FakeClock, InMemorySessionRepo, InMemorySessionStorage, SystemClock,
};
pub use reducer::{
    validate_record_log, RecordLogCorruption, RecordLogCorruptionReason, RecordLogSlice,
};
pub use session::{DefaultIdGenerator, Session};
pub use sqlite::{SqliteSessionRepo, SqliteSessionStorage};
pub use state::{ApplyOutcome, SessionState};
pub use values::{SessionValueWriter, SessionValues, VALUE_CUSTOM_TYPE};
pub mod keyed_queue;
pub mod resources;
pub mod search;
