//! Mirrors `packages/agent/src/harness` (tools + env + session + compaction).
//!
//! M4 scope: `ExecutionEnv` trait + `OsExecutionEnv` + `InMemoryExecutionEnv` +
//! file-mutation-queue + read/write/edit/bash tools + edit-diff + utils
//! (truncate, shell-output, image, path-utils). Session/compaction arrive in M5
//! (these live in `pi-harness`, not here).
//!
//! Workspace dep chain: `pi-telemetry → pi-ai → pi-agent → pi-tools →
//! pi-harness → pi-cli`. `pi-tools` depends on `pi-agent` (for the `AgentTool`
//! trait) and `pi-ai` (for `Tool`/content types).

#![forbid(unsafe_code)]

pub mod env;
pub mod error;
pub mod file_io;
pub mod file_mutation_queue;
pub mod image;
pub mod image_processing;
pub mod in_memory;
pub mod os_env;
pub mod output_decode;
pub mod path_utils;
pub mod search;
pub mod shell_output;
pub mod truncate;

pub mod utils {
    pub mod git;
    pub mod open_browser;
}

pub mod tools;
pub use tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_powershell_tool, create_read_tool, create_write_tool, BashExecution, BashPrepare,
    BashToolOptions, ExecutionToolContext, ProcessedImage, ReadImageProcessor, ReadToolOptions,
};

pub use env::{
    ExecutionEnv, FileContent, FileInfo, FileKind, FileSystem, Shell, ShellExecOptions,
    ShellOutput, ShellOutputChunk,
};
pub use error::{
    io_to_file_code, io_to_file_error, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode,
};
pub use file_mutation_queue::{
    get_mutation_queue_key, with_file_mutation_queue, MutatingEnv, MutationQueueRegistry,
};
pub use image::{detect_supported_image_mime_type, encode_base64};
pub use in_memory::{InMemoryExecutionEnv, ShellScript};
pub use os_env::{is_host_private_env, OsExecutionEnv, HOST_PRIVATE_ENV};
pub use path_utils::{normalize_tool_path, resolve_read_tool_path, resolve_tool_path};
pub use truncate::{
    format_size, split_lines_for_counting, truncate_head, truncate_line, truncate_tail,
    TruncateLineOutput, TruncationLimit, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH,
};
