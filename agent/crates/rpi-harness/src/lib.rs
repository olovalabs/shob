//! Mirrors `packages/agent/src/harness` (AgentHarness: session tree, JSONL
//! persistence, compaction, the run loop that drives `pi-agent`).
//!
//! M5 surface, built up module-by-module. Currently exposes the foundation
//! layers (`error`, `result`, `types`, `session::types`); subsequent modules
//! (`session::state`, `session::reducer`, `session::jsonl`, `compaction`,
//! `messages`, `events`, `skills`, `agent_harness`) are added as M5 progresses.

#![forbid(unsafe_code)]

pub mod agent_harness;
pub mod cache_stats;
pub mod compaction;
pub mod context_files;
pub mod diagnostics;
pub mod error;
pub mod events;
pub mod frame_progress;
pub mod frontmatter;
mod image_tool;
pub mod messages;
pub mod prompt_templates;
pub mod result;
pub mod run_budget;
pub mod runtime;
pub mod session;
pub mod settle;
pub mod skills;
pub mod system_prompt;
mod tool_execution;
pub mod tool_program_body;
mod tool_program_journal;
mod tool_program_observers;
pub mod tool_program_queries;
pub mod tool_recovery;
pub mod types;
pub mod watcher;
