//! Text markers for tool results that recovery produced rather than the loop.
//!
//! Kept apart from `frame_progress` because they belong to the *tool* half of
//! recovery: `frame_progress` decides what the model had committed, this module
//! says what to put in a tool result that the original attempt never wrote.

/// Appended to a tool result that recovery produced by re-running the tool.
///
/// The model and the user both need to know this result came from a re-run, not
/// from the original attempt: the interruption may have left the environment in a
/// state the re-run did not observe.
pub const REPLAYED_TOOL_RESULT: &str =
    "[recovered] This tool call was re-run after the process was interrupted; the result below is \
from that re-run.";
