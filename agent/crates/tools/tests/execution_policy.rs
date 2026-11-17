use agent::ToolExecutionMode;
use std::sync::Arc;
use tools::{ExecutionToolContext, InMemoryExecutionEnv};

#[test]
fn audited_read_only_tools_opt_into_parallel_execution() {
    let env = Arc::new(InMemoryExecutionEnv::new());
    let context = ExecutionToolContext::new(env.clone(), Some(env));
    let tools = [
        tools::create_read_tool(&context, None),
        tools::create_find_tool(&context, None),
        tools::create_grep_tool(&context, None),
        tools::create_ls_tool(&context, None),
        tools::tools::create_image_tool(),
    ];
    for tool in tools {
        assert_eq!(
            tool.execution_mode(),
            ToolExecutionMode::Parallel,
            "{}",
            tool.schema().name
        );
    }
}

#[test]
fn file_mutations_remain_exclusive() {
    let env = Arc::new(InMemoryExecutionEnv::new());
    let context = ExecutionToolContext::new(env.clone(), Some(env));
    for tool in [
        tools::create_write_tool(&context),
        tools::create_edit_tool(&context),
    ] {
        assert_eq!(
            tool.execution_mode(),
            ToolExecutionMode::Sequential,
            "{}",
            tool.schema().name
        );
    }
}
