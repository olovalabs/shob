//! Concrete providers. Mirrors `packages/ai/src/api/*` + the in-tree faux
//! provider (`packages/agent/src/faux.ts`, hoisted here so every crate's tests
//! can reuse it without depending on `pi-agent`).
//!
//! Faux is always available. HTTP providers are enabled by the `providers`
//! feature.

pub mod faux;

#[cfg(feature = "providers")]
pub mod anthropic;

#[cfg(feature = "providers")]
pub mod antigravity;

#[cfg(feature = "providers")]
pub mod openai_completions;

#[cfg(feature = "providers")]
pub mod openai_responses;

#[cfg(feature = "providers")]
pub mod google;

#[cfg(feature = "providers")]
pub mod bedrock;

#[cfg(feature = "providers")]
pub mod azure;

#[cfg(feature = "providers")]
pub mod cursor;

#[cfg(feature = "providers")]
pub mod devin;

#[cfg(feature = "providers")]
pub mod routed;

#[cfg(feature = "providers")]
mod request;

// ---------------------------------------------------------------------------
// Streaming delta batching (shared policy)
// ---------------------------------------------------------------------------

/// Bytes accumulated before a batched text/thinking delta is emitted.
///
/// Every `AssistantMessageEvent` carries a full `partial: Arc<AssistantMessage>`
/// snapshot, so emitting one event per wire delta made a long stream quadratic
/// in the message length: measured, 8000 one-byte deltas cost ~105ms and the
/// scaling was superlinear. Batching keeps per-event work proportional to the
/// *batch* rather than to the whole message so far.
///
/// Channel boundaries always flush (`TextEnd`, `ThinkingEnd`, `ToolCallEnd`,
/// `Done`/`Error`, and a switch between text and thinking), so this changes only
/// how smoothly text appears — never the final message, and never the
/// concatenation of the deltas a consumer sees.
#[cfg(feature = "providers")]
pub(crate) const DELTA_FLUSH_BYTES: usize = 64;

/// Flush the first delta immediately. After that, amortize snapshots during
/// bursts and release a pending batch within roughly a quarter of a 60 Hz frame.
#[cfg(feature = "providers")]
pub(crate) const DELTA_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);
