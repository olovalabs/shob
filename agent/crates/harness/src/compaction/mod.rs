//! Mirrors `packages/agent/src/harness/compaction` — context-window management
//! for long sessions. When token usage approaches the model's context window,
//! older history is summarized into a `compactionSummary` entry; branch tails
//! folded back into a conversation are summarized into a `branchSummary` entry.
//!
//! Submodules:
//! - [`settings`] — `should_compact` threshold + re-export of
//!   `CompactionSettings`/`DEFAULT_COMPACTION_SETTINGS` (which live in
//!   [`crate::types`]).
//! - [`tokens`] — `estimate_tokens`/`estimate_context_tokens`, file-operation
//!   primitives, `serialize_conversation`, and the minimal `build_session_context`
//!   compaction needs for `tokensBefore`.
//! - [`cut_point`] — `find_valid_cut_points`/`find_turn_start_index`/
//!   `find_cut_point`.
//! - [`compaction`] — `CompactionError`, `CompactionPreparation`,
//!   `prepare_compaction`, `compact` (split-turn TWO LLM calls),
//!   `complete_simple_with_retries`, prompts, `extract_file_operations`.
//! - [`branch_summary`] — `collect_entries_for_branch_summary`,
//!   `prepare_branch_entries`, `generate_branch_summary`.

pub mod branch_summary;
pub mod compaction;
pub mod cut_point;
pub mod settings;
pub mod tokens;

pub use compaction::{
    combine_usage, compact, complete_simple_with_retries, extract_file_operations,
    generate_summary_with_usage, generate_turn_prefix_summary, get_message_from_entry,
    get_message_from_entry_for_compaction, is_retryable_assistant_error, prepare_compaction,
    CompactResult, CompactionDetails, CompactionError, CompactionLlmOptions, CompactionPreparation,
};
pub use cut_point::{find_cut_point, find_turn_start_index, find_valid_cut_points, CutPointResult};
pub use settings::{should_compact, CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
pub use tokens::{
    build_session_context, compute_file_lists, estimate_context_tokens, estimate_tokens,
    extract_file_ops_from_message, format_file_operations, serialize_conversation,
    ContextUsageEstimate, FileOperations, SessionContext,
};
