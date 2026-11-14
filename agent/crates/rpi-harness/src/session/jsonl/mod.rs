//! `session::jsonl` — mirrors `packages/agent/src/harness/session/jsonl/` (the
//! v4 JSONL codec + durable file `SessionStorage` + repo).
//!
//! Submodules:
//! - [`errors`] — `JsonlDecodeError` + `file_result` / `invalid_file`.
//! - [`types`] — `JsonlV4Header`, `JsonlSessionMetadata`, repo options.
//! - [`codec`] — v4 line codec (header + mutation parse/encode).
//! - [`storage`] — `JsonlSessionStorage` (torn-tail recovery + atomic publish).
//! - [`repo`] — `JsonlSessionRepo` (`SessionRepo` impl over JSONL files).

pub mod codec;
mod cold_decode;
mod cold_frames;
pub mod errors;
mod progress;
pub mod repo;
pub mod storage;
pub mod types;

pub use codec::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
};
pub use errors::{file_result, invalid_file, JsonlDecodeError, JsonlDecodeErrorKind};
pub use repo::JsonlSessionRepo;
pub use storage::{JsonlSessionFacts, JsonlSessionStorage};
pub use types::{
    HeaderKind, JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlSourceFormat, JsonlV4Header,
};
