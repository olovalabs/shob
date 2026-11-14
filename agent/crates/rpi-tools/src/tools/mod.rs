//! Mirrors `packages/agent/src/harness/tools/*` — the built-in `read`/`write`/
//! `edit`/`bash` `AgentTool` impls + the read-only `grep`/`find`/`ls` ports
//! (port of `packages/coding-agent/src/core/tools/{grep,find,ls}.ts`) + their
//! shared helpers.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod external;
pub mod find;
pub mod grep;
pub mod image;
pub mod ls;
pub mod powershell;
pub mod read;
pub mod task_list;
pub mod tool_context;
pub mod user_input;
pub mod write;

pub use bash::{create_bash_tool, BashExecution, BashPrepare, BashToolOptions};
pub use edit::create_edit_tool;
pub use external::{
    create_external_tool, validate_external_tool_definitions, ExternalToolBackend,
    ExternalToolDefinition,
};
pub use find::create_find_tool;
pub use grep::create_grep_tool;
pub use image::{create_image_tool, create_image_tool_with_config, ImageToolConfig};
pub use ls::create_ls_tool;
pub use powershell::create_powershell_tool;
pub use read::{create_read_tool, ProcessedImage, ReadImageProcessor, ReadToolOptions};
pub use tool_context::ExecutionToolContext;
pub use write::create_write_tool;
