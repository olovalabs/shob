//! Mirrors `packages/agent/src/harness/session/jsonl/types.ts` — the JSONL
//! repo's typed options + metadata + v4 header shape.
//!
//! The TS `JsonlSessionRepoFileSystem` is a `Pick` of the harness `FileSystem`
//! trait; the Rust port reuses [`rpi_tools::env::FileSystem`] directly (it
//! already exposes every method the Pick selected), so there is no separate
//! file-system trait here.

use std::sync::Arc;

use rpi_tools::env::FileSystem;

use crate::session::memory::Clock;
use crate::session::types::{IdGenerator, JsonValue, SessionCreateOptions, SessionMetadata};

/// Marker that serializes as the literal string `"header"` (the v4 header's
/// `kind` discriminator). Decoding is manual in [`super::codec`], so only
/// `Serialize` is exercised (by `encode_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderKind;

impl serde::Serialize for HeaderKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("header")
    }
}

/// `sourceFormat: 3 | 4`. The v4 codec always writes `4`; `3` is preserved on
/// read-only legacy metadata for compatibility with the TS repo, which can
/// surface v3 files. We model it as a tiny enum rather than a `u8` so the
/// "3 or 4" constraint is expressed in the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlSourceFormat {
    V3,
    V4,
}

impl JsonlSourceFormat {
    pub fn as_u8(self) -> u8 {
        match self {
            JsonlSourceFormat::V3 => 3,
            JsonlSourceFormat::V4 => 4,
        }
    }
}

/// Mirrors TS `JsonlV4Header`. Serialized camelCase to match the on-disk v4
/// wire shape exactly (so Rust-written files are interchangeable with the TS
/// impl). `parentSessionId` / `legacyParentSessionPath` are mutually exclusive
/// (validated in [`super::codec::parse_header`]).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlV4Header {
    pub kind: HeaderKind,
    pub version: u32,
    pub id: String,
    pub created_at: i64,
    pub cwd: String,
    /// A required recovery companion; absent only on history-only artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Preserved only when a v3 parent path could not be resolved to a session
    /// id — mutually exclusive with `parent_session_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_parent_session_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, JsonValue>>,
}

/// Mirrors TS `JsonlSessionMetadata` (extends `SessionMetadata` with JSONL-
/// specific filesystem fields). Carries the on-disk `path` + `cwd` +
/// `modified_at` so the repo/storage can locate and append to the file without
/// re-deriving them. The shared [`SessionStorage`] trait exposes only the base
/// [`SessionMetadata`]; the richer fields live here for the JSONL backend.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonlSessionMetadata {
    pub id: String,
    pub created_at: i64,
    pub cwd: String,
    pub path: String,
    /// Filesystem modification time as milliseconds since Unix epoch.
    pub modified_at: i64,
    pub source_format: JsonlSourceFormat,
    pub parent_session_id: Option<String>,
    /// Present only when a v3 parent path could not be resolved to a session id.
    pub legacy_parent_session_path: Option<String>,
    /// Opaque application-owned metadata.
    pub metadata: Option<serde_json::Map<String, JsonValue>>,
}

impl JsonlSessionMetadata {
    /// Project to the shared base [`SessionMetadata`] (the subset the
    /// [`SessionStorage`] trait exposes).
    pub fn to_base(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.id.clone(),
            created_at: self.created_at,
            parent_session_id: self.parent_session_id.clone(),
        }
    }
}

/// Mirrors TS `JsonlSessionRepoOptions`. `fs` is the shared
/// [`rpi_tools::env::FileSystem`] (the Rust analog of the TS `Pick<FileSystem,…>`).
///
/// The Rust port additionally carries a shared [`Clock`] + [`IdGenerator`]: the
/// TS repo reaches into `Date.now()` / `uuidv7()` directly, but the Rust harness
/// abstracts both behind injectable traits (so tests can run deterministically
/// against the in-memory FS). Passing them through the repo options keeps every
/// `SessionStorage` construction (create/load/fork) supplied consistently.
#[derive(Clone)]
pub struct JsonlSessionRepoOptions {
    pub fs: Arc<dyn FileSystem>,
    /// Root containing cwd-encoded session directories.
    pub sessions_root: String,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

/// Mirrors TS `JsonlSessionCreateOptions` (extends `SessionCreateOptions` with
/// `cwd` + `metadata`). The shared [`SessionCreateOptions`] now carries `cwd`
/// + `metadata` as `Option`s (ignored by the in-memory repo); this struct is
/// kept as a typed convenience for the JSONL repo's inherent API so `cwd` is
/// non-optional at the JSONL entry point.
#[derive(Debug, Clone)]
pub struct JsonlSessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
    pub cwd: String,
    pub metadata: Option<serde_json::Map<String, JsonValue>>,
}

impl JsonlSessionCreateOptions {
    /// Build from a shared [`SessionCreateOptions`] + the cwd/metadata the
    /// JSONL repo additionally requires.
    pub fn from_shared(base: &SessionCreateOptions, cwd: String) -> Self {
        Self {
            id: base.id.clone(),
            parent_session_id: base.parent_session_id.clone(),
            cwd,
            metadata: base.metadata.clone(),
        }
    }
}

/// Mirrors TS `JsonlSessionListOptions { cwd? }`. The shared `SessionRepo`
/// trait has no list options, so this is used only by the JSONL repo's inherent
/// typed `list` API.
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionListOptions {
    pub cwd: Option<String>,
}
