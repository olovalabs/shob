//! Mirrors the abort surface of `packages/agent/src/agent.ts` /
//! `packages/agent/src/agent-loop.ts`. TS threads an `AbortSignal` through
//! every layer; Rust uses `tokio_util::sync::CancellationToken` instead.
//!
//! [`AbortHandle`] is the `Agent`-level wrapper: it owns the root token for a
//! run, exposes `abort()`, and hands out child tokens to nested operations
//! (tool execution, provider stream) so a single `cancel()` propagates to
//! every in-flight future without the caller having to thread it manually.

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Owns the cancellation root for one agent run. Clones share the same root,
/// so `abort()` on any clone cancels the whole tree. Mirrors the
/// `AbortController` held per-`Agent`-run in TS.
#[derive(Clone)]
pub struct AbortHandle {
    token: CancellationToken,
}

impl AbortHandle {
    /// A fresh, un-cancelled handle with its own root.
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Cancel the run. Idempotent — further calls are no-ops. Mirrors
    /// `controller.abort()`.
    pub fn abort(&self) {
        self.token.cancel();
    }

    /// True once `abort()` has been called on this tree.
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The underlying token. Use this to `select!` against a long future.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// A child token — cancel when the parent cancels, but can also be
    /// cancelled independently (e.g. a per-tool timeout) without affecting
    /// siblings. This is how the loop gives each tool its own abort scope
    /// while still honouring an agent-wide `abort()`.
    pub fn child(&self) -> CancellationToken {
        self.token.child_token()
    }
}

impl Default for AbortHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap an externally-owned token (used by `run_agent_loop` when a caller
/// supplies their own `CancellationToken` rather than an `AbortHandle`).
pub fn from_token(token: CancellationToken) -> AbortHandle {
    AbortHandle { token }
}

/// A shared abort handle wrapped in `Arc` — the shape `Agent` stores so it can
/// hand clones to `prompt`/`continue_run` callers and the run loop alike.
pub type SharedAbortHandle = Arc<AbortHandle>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_propagates_to_children() {
        let handle = AbortHandle::new();
        let child = handle.child();
        assert!(!child.is_cancelled());
        handle.abort();
        // child_token() returns a token that's cancelled when the parent is.
        assert!(handle.is_aborted());
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancel_does_not_cancel_parent() {
        let handle = AbortHandle::new();
        let child = handle.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!handle.is_aborted());
    }
}
