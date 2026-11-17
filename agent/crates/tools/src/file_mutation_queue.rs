//! Mirrors `packages/agent/src/harness/tools/file-mutation-queue.ts` — serializes
//! file-mutating operations per canonical path so concurrent edits to the same
//! path (or a symlink + its target) don't interleave.
//!
//! The TS uses a `WeakMap<ExecutionEnv, MutationQueueState>` keyed by env
//! instance. Rust has no per-instance map keyed by trait object, so each env
//! impl carries an `Arc<MutationQueueRegistry>` field and exposes it via the
//! [`MutatingEnv`] accessor trait. The mutation-dependent tools (write, edit)
//! hold `Arc<dyn MutatingEnv>`; read and bash don't need it.
//!
//! **Critical invariant (plan §5.4): abort does NOT unblock the queue.** An
//! aborted caller still waits for its turn, then runs `fn` (which may observe
//! the abort and return `Err(Aborted)`), then releases. The lock guard is held
//! across `fn().await`; a `CancellationToken` only causes `fn` to error out —
//! it does NOT preempt mutex acquisition.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::{FileError, FileErrorCode};

/// Per-env registry of canonical-path → mutex. Stored as a field on each env
/// impl (behind `Arc` so clones share the same map).
#[derive(Default)]
pub struct MutationQueueRegistry {
    queues: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MutationQueueRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or lazily create) the mutex for `key`. The key is a canonical path
    /// string (from [`get_mutation_queue_key`]).
    async fn mutex_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.queues.lock().await;
        if let Some(m) = map.get(key) {
            m.clone()
        } else {
            let m = Arc::new(Mutex::new(()));
            map.insert(key.to_string(), m.clone());
            m
        }
    }
}

/// Accessor used by [`with_file_mutation_queue`] to reach the env's registry
/// AND its `ExecutionEnv` methods. Concrete env impls (`OsExecutionEnv`,
/// `InMemoryExecutionEnv`) implement this; the write/edit tools hold
/// `Arc<dyn MutatingEnv>`.
///
/// The `as_env` indirection avoids needing trait-upcasting (`dyn MutatingEnv`
/// → `dyn ExecutionEnv`, unstable before Rust 1.86); our min rust-version is
/// 1.78, so we expose the env explicitly.
pub trait MutatingEnv: Send + Sync {
    /// The underlying `ExecutionEnv` (for `FileSystem`/`Shell` calls).
    fn as_env(&self) -> &dyn ExecutionEnv;

    /// The registry holding per-canonical-path mutexes.
    fn mutation_registry(&self) -> &MutationQueueRegistry;
}

/// Compute the canonical-path queue key for `path`. Mirrors
/// `getMutationQueueKey`:
/// 1. `absolutePath(path)` → propagate err.
/// 2. `canonicalPath(absolute)`:
///    - `Ok` → return canonical string.
///    - `Err` with `not_found` or `not_supported` → fall back to absolute string.
///    - other err → propagate.
pub async fn get_mutation_queue_key(
    env: &dyn ExecutionEnv,
    path: &str,
    cancel: Option<&CancellationToken>,
) -> Result<String, FileError> {
    let absolute = env.absolute_path(path, cancel).await?;
    let abs_str = absolute.to_string_lossy().into_owned();
    match env.canonical_path(&abs_str, cancel).await {
        Ok(canonical) => Ok(canonical.to_string_lossy().into_owned()),
        Err(e) if e.code == FileErrorCode::NotFound || e.code == FileErrorCode::NotSupported => {
            Ok(abs_str)
        }
        Err(e) => Err(e),
    }
}

/// Serialize `fn` per canonical-path key for `env`. The closure `f` runs while
/// holding the per-key `tokio::Mutex` guard; abort/cancel during the wait does
/// NOT skip the queue — `f` itself must observe the token and return
/// `Err(FileError::aborted())`.
///
/// Mirrors `withFileMutationQueue`. The `&CancellationToken` is passed to `f`
/// so it can perform the same pre/post-execution abort checks the tools do
/// (the write tool checks before AND after the write).
pub async fn with_file_mutation_queue<F, R>(
    env: &dyn MutatingEnv,
    path: &str,
    cancel: &CancellationToken,
    f: F,
) -> Result<R, FileError>
where
    F: for<'a> FnOnce(
        &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<R, FileError>>,
    R: Send + 'static,
{
    let key = get_mutation_queue_key(env.as_env(), path, Some(cancel)).await?;
    let mutex = env.mutation_registry().mutex_for(&key).await;
    // Acquisition of the per-key lock is NOT cancelable — we always wait for
    // our turn. `tokio::Mutex::lock` is a safe `.await` here.
    let _guard = mutex.lock().await;
    // Now that we hold the lock, run `f`. It may observe cancellation and
    // return Err(Aborted); the guard drops on return, releasing the next
    // waiter. Unwinding (if `f` panics) also drops the guard — but tool/env
    // code must never panic (plan §5.13).
    f(cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_creates_and_reuses_mutex() {
        let reg = MutationQueueRegistry::new();
        let m1 = reg.mutex_for("/a/b").await;
        let m2 = reg.mutex_for("/a/b").await;
        assert!(Arc::ptr_eq(&m1, &m2));
        let m3 = reg.mutex_for("/c").await;
        assert!(!Arc::ptr_eq(&m1, &m3));
    }
}
