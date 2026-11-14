//! Mirrors `packages/agent/src/stream-fn.ts` + the `StreamFn` type alias in
//! `packages/agent/src/types.ts`.
//!
//! TS `StreamFn = (model, context, options?) => AssistantMessageEventStream |
//! Promise<AssistantMessageEventStream>` — note it may return the stream
//! *synchronously*. The agent loop never awaits the call itself; it only awaits
//! `stream.next()` / `stream.result()`. The Rust port models the same contract:
//! the closure returns the consumer synchronously and a producer task is
//! already running by the time it returns.
//!
//! `AgentBuilder` lets callers install an explicit `StreamFn`. When omitted,
//! `get_default_stream_fn` is used; like TS, it errors if no default has been
//! installed via `set_default_stream_fn` — `pi-ai` providers are not silently
//! picked so the agent crate stays free of provider wiring.

use rpi_ai::provider::SimpleStreamOptions;
use rpi_ai::types::Context;
use rpi_ai::{AssistantMessageEventStream, Model};
use std::sync::{Arc, OnceLock};

/// The provider-boundary callable. Arc'd so `Agent`/`AgentLoopConfig` can clone
/// it cheaply and pass it by reference into `run_agent_loop`.
///
/// Implementations MUST NOT panic or return `Err`: request/model/runtime
/// failures are encoded as `Error` events on the returned stream (mirrors the
/// TS contract). The closure returns synchronously; the producer task it
/// spawns is already detached.
pub type StreamFn = Arc<
    dyn Fn(&Model, &Context, &SimpleStreamOptions) -> AssistantMessageEventStream + Send + Sync,
>;

/// Build a `StreamFn` from any `Fn(&Model,&Context,&SimpleStreamOptions) ->
/// AssistantMessageEventStream + Send + Sync`.
pub fn stream_fn<F>(f: F) -> StreamFn
where
    F: Fn(&Model, &Context, &SimpleStreamOptions) -> AssistantMessageEventStream
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

static DEFAULT_STREAM_FN: OnceLock<StreamFn> = OnceLock::new();

/// Install the process-wide fallback used by `Agent`/`AgentBuilder` when no
/// `StreamFn` is supplied. Mirrors TS `setDefaultStreamFn`.
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    // OnceLock: first writer wins. A `None` install is treated as "leave as-is"
    // if already set, matching TS where the field can be cleared by setting
    // undefined but only before first use; here we simply no-op a None install
    // when a default already exists.
    if let Some(f) = stream_fn {
        let _ = DEFAULT_STREAM_FN.set(f);
    }
}

/// Return the installed default, or `None` if none has been installed. Used by
/// `AgentBuilder::build` to decide whether to defer resolution to call time.
pub fn try_get_default_stream_fn() -> Option<&'static StreamFn> {
    DEFAULT_STREAM_FN.get()
}

/// Return the installed default. Mirrors TS `getDefaultStreamFn` — but instead
/// of throwing we return `None`; the caller (`run_agent_loop`) surfaces a
/// clean `AgentError::State` when a default is required and absent.
pub fn get_default_stream_fn() -> Result<StreamFn, crate::AgentError> {
    DEFAULT_STREAM_FN
        .get()
        .map(|f| Arc::clone(f))
        .ok_or_else(|| {
            crate::AgentError::State(
                "no default stream fn configured — pass one to AgentBuilder or call \
                 set_default_stream_fn"
                    .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_fn_is_arc_clone() {
        let f = stream_fn(|_, _, _| {
            let (_prod, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
            stream
        });
        let _clone: StreamFn = Arc::clone(&f);
    }
}
