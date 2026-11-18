use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use ai::types::{Schema, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

struct ConcurrentRead {
    schema: Tool,
    barrier: Barrier,
    started: AtomicUsize,
}

impl ConcurrentRead {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            schema: Tool {
                name: "read".into(),
                description: String::new(),
                parameters: Schema::new(
                    json!({"type":"object", "properties":{"index":{"type":"integer"}}, "required":["index"]}),
                ),
                constrained_sampling: None,
            },
            barrier: Barrier::new(4),
            started: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl AgentTool for ConcurrentRead {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "read"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }
    async fn execute(
        &self,
        _: &str,
        arguments: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.started.fetch_add(1, Ordering::Relaxed);
        self.barrier.wait().await;
        let index = arguments["index"].as_u64().unwrap();
        let result = AgentToolResult {
            content: vec![TextContentOrImage::text(format!("file-{index}"))],
            details: json!({"source": index}),
            ..Default::default()
        };
        if index == 2 {
            Err(AgentError::StructuredTool(Box::new(result)))
        } else {
            Ok(result)
        }
    }
}

#[tokio::test]
async fn overlaps_independent_reads_preserving_order_and_structured_failures() {
    let read = ConcurrentRead::new();
    let metrics = Arc::new(tools::metrics::ToolMetrics::default());
    let measured = tools::metrics::measure_tool(read.clone(), metrics.clone());
    let batch = tools::metrics::measure_tool(tools::create_batch_tool(vec![measured]), metrics);
    let calls: Vec<Value> = (0..4)
        .map(|index| json!({"tool":"read", "index":index}))
        .collect();
    let result = batch
        .execute(
            "batch-1",
            json!({"calls":calls}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let TextContentOrImage::Text(text) = &result.content[0] else {
        panic!("text result")
    };
    let items: Vec<Value> = serde_json::from_str(&text.text).unwrap();
    assert_eq!(read.started.load(Ordering::Relaxed), 4);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(
            item["result"]["content"][0]["text"],
            format!("file-{index}")
        );
        assert_eq!(item["is_error"], index == 2);
        assert_eq!(item["result"]["details"]["source"], index);
        assert_eq!(
            item["result"]["details"]["performance"]["displayed_text_bytes"],
            6
        );
    }
    assert_eq!(result.details["performance"]["session_executions"], 5);
}

#[tokio::test]
async fn validates_every_child_before_starting_and_rejects_disabled_or_mutating_tools() {
    let read = ConcurrentRead::new();
    let batch = tools::create_batch_tool(vec![read.clone()]);
    for calls in [
        json!([{"tool":"read","index":0}, {"tool":"read"}]),
        json!([{"tool":"grep","pattern":"error"}]),
        json!([{"tool":"bash","command":"touch unexpected"}]),
        json!([]),
        json!((0..5)
            .map(|index| json!({"tool":"read","index":index}))
            .collect::<Vec<_>>()),
    ] {
        let result = batch
            .execute(
                "invalid",
                json!({"calls": calls}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await;
        assert!(matches!(result, Err(AgentError::Validation(_))));
        assert_eq!(read.started.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn oversized_item_is_an_explicit_failure_not_a_truncated_success() {
    let env = Arc::new(tools::InMemoryExecutionEnv::new());
    env.seed_file(
        "/image.png",
        [
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".as_slice(),
            &vec![0; 200_000],
        ]
        .concat(),
    )
    .await;
    let context = tools::ExecutionToolContext::new(env, None);
    let batch = tools::create_batch_tool(vec![tools::create_read_tool(&context, None)]);
    let result = batch
        .execute(
            "large",
            json!({"calls":[{"tool":"read","path":"/image.png"}]}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let TextContentOrImage::Text(text) = &result.content[0] else {
        panic!("text result")
    };
    let items: Vec<Value> = serde_json::from_str(&text.text).unwrap();
    assert_eq!(items[0]["is_error"], true);
    assert!(items[0]["error"].as_str().unwrap().contains("standalone"));
}

#[tokio::test]
async fn flat_calls_pass_provider_validation_and_read_the_requested_files() {
    use tools::FileSystem;

    let env = Arc::new(tools::InMemoryExecutionEnv::new());
    for (path, contents) in [
        ("/package.json", "{\"name\":\"batch-fixture\"}"),
        ("/nuxt.config.ts", "export default defineNuxtConfig({})"),
        ("/README.md", "# Fixture project"),
        ("/app/page.vue", "<template>fixture page</template>"),
    ] {
        env.seed_file(path, contents.as_bytes().to_vec()).await;
    }
    env.create_dir("/app", true, None).await.unwrap();
    let context = tools::ExecutionToolContext::new(env, None);
    let batch = tools::create_batch_tool(vec![
        tools::create_read_tool(&context, None),
        tools::create_ls_tool(&context, None),
        tools::create_grep_tool(&context, None),
    ]);
    let validator = jsonschema::JSONSchema::compile(&batch.schema().parameters.0).unwrap();
    for (input, expected) in [
        (
            json!({"calls": [
                {"tool":"read","path":"package.json"},
                {"tool":"read","path":"nuxt.config.ts"},
                {"path":"README.md","tool":"read"},
                {"path":"app","tool":"ls"}
            ]}),
            vec![
                "batch-fixture",
                "defineNuxtConfig",
                "# Fixture project",
                "page.vue",
            ],
        ),
        (
            json!({"calls": [{"tool":"grep","pattern":"batch-fixture","mode":"files"}]}),
            vec!["package.json"],
        ),
    ] {
        assert!(validator.is_valid(&input), "{input}");
        let result = batch
            .execute("flat", input, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        let TextContentOrImage::Text(text) = &result.content[0] else {
            panic!("batch text");
        };
        let items: Vec<Value> = serde_json::from_str(&text.text).unwrap();
        for (item, expected) in items.iter().zip(expected) {
            assert_eq!(item["is_error"], false, "{item}");
            assert!(item["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expected));
        }
    }
    for invalid in [
        json!({"calls": [{"tool":"read"}]}),
        json!({"calls": [{"tool":"grep","pattern":"fixture","mode":"invalid"}]}),
        json!({"calls": [{"tool":"bash","command":"touch unexpected"}]}),
        json!({"calls": [{"tool":"read","arguments":{"path":"package.json"}}]}),
    ] {
        assert!(!validator.is_valid(&invalid), "{invalid}");
    }
}
