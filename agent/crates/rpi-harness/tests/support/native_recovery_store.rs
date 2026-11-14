//! Default-feature tests compile the same engine used by the opt-in backend.
use rpi_harness::session as facts;
#[path = "../../src/session/experimental_native/codec.rs"]
mod codec;
#[allow(dead_code)] // The shared engine also exposes opt-in adapter/diagnostic paths.
#[path = "../../src/session/experimental_native/engine.rs"]
mod engine;
#[allow(dead_code)] // Query matching is exercised through the opt-in trait adapter.
#[path = "../../src/session/experimental_native/selection.rs"]
mod selection;
pub use engine::*;

#[allow(dead_code)]
#[path = "../../src/session/experimental_native/index.rs"]
mod index;
#[allow(dead_code)]
#[path = "../../src/session/message_preview.rs"]
mod preview;
