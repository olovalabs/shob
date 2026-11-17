//! Opt-in, explicitly constructed experimental native row store.
//!
//! Only explicit construction / fresh print-only CLI opt-in; no Web routing,
//! discovery of legacy sessions, migration, or stable format promise.
use crate::session as facts;
use crate::session::message_preview as preview;
mod codec;
#[allow(dead_code)] // Internal stamped-request probes remain shared with default-feature tests.
mod engine;
mod index;
mod repo;
mod selection;
mod storage;
pub use repo::ExperimentalNativeRepo;
pub use storage::ExperimentalNativeStorage;
mod pages;
mod queries;
