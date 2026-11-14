//! Mirrors `packages/agent/src/harness/tools/tool-context.ts` — the context the
//! built-in tools execute against.
//!
//! TS: `interface ExecutionToolContext { env: ExecutionEnv }`. The Rust port
//! keeps the same single-field shape but splits `env` into the read-only
//! `ExecutionEnv` view and (for the write/edit tools) the `MutatingEnv` view
//! that carries the file-mutation-queue registry.

use std::sync::Arc;

use crate::env::ExecutionEnv;
use crate::file_io::FileIORouter;
use crate::file_mutation_queue::MutatingEnv;

/// The context handed to a built-in tool's `execute`. Mirrors TS
/// `ExecutionToolContext`.
///
/// `env` is the read-only view for read/bash; `mutating_env` is `Some` when the
/// context was built with a mutation-capable env (write/edit use it to reach
/// the per-canonical-path queue). Callers conventionally wrap an
/// `OsExecutionEnv` or `InMemoryExecutionEnv` (which implement both traits)
/// via [`ExecutionToolContext::new`].
pub struct ExecutionToolContext {
    env: Arc<dyn ExecutionEnv>,
    mutating_env: Option<Arc<dyn MutatingEnv>>,
    file_io: Option<Arc<FileIORouter>>,
}

impl ExecutionToolContext {
    /// Build a context from a mutation-capable env. The env is stored both as
    /// `Arc<dyn ExecutionEnv>` (for read/bash) and `Arc<dyn MutatingEnv>` (for
    /// write/edit). The two arcs point at the same allocation when the caller
    /// passes a single concrete env.
    pub fn new(env: Arc<dyn ExecutionEnv>, mutating_env: Option<Arc<dyn MutatingEnv>>) -> Self {
        Self {
            env,
            mutating_env,
            file_io: None,
        }
    }

    /// Attach optional neutral text delegation. The shared router starts local;
    /// its owner may configure a backend later, only while the session is idle.
    pub fn with_file_io(mut self, router: Arc<FileIORouter>) -> Self {
        self.file_io = Some(router);
        self
    }

    pub fn file_io(&self) -> Option<&Arc<FileIORouter>> {
        self.file_io.as_ref()
    }

    pub fn env(&self) -> &Arc<dyn ExecutionEnv> {
        &self.env
    }

    pub fn mutating_env(&self) -> Option<&Arc<dyn MutatingEnv>> {
        self.mutating_env.as_ref()
    }
}
