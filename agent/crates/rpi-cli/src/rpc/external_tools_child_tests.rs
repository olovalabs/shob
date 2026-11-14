use super::super::writer::start_rpc_writer;
use rpi_agent::tool_execution_scope::ToolExecutionScope;
use rpi_tools::tools::external::{create_external_tool, ExternalToolDefinition};

struct ExternalEventCapture(tokio::sync::mpsc::UnboundedSender<Value>);
impl std::io::Write for ExternalEventCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            self.0.send(serde_json::from_slice(line)?).unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn external_test_tool(backend: Arc<ExternalTools>) -> Arc<dyn rpi_agent::agent_tool::AgentTool> {
    create_external_tool(
        ExternalToolDefinition {
            name: "external_test".into(),
            description: "test".into(),
            input_schema: json!({"type":"object"}),
        },
        backend,
    )
}

#[tokio::test]
async fn child_scope_rejects_external_tools_without_emitting_even_with_spoofed_arguments() {
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (writer, task, _) = start_rpc_writer(ExternalEventCapture(tx), Default::default());
    let backend = ExternalTools::detached(writer);
    let tool = external_test_tool(backend.clone());
    // Real host scope, including a hostile false childAgent in the context.
    // Neither that value nor the caller's JSON may override host provenance.
    let scope = Arc::new(ToolExecutionScope::with_context(
        json!({"childAgent":false}),
    ));
    let result = scope
        .run(tool.execute(
            "child-call",
            json!({"childAgent":false,"sessionId":"root"}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        ))
        .await;
    assert!(matches!(result, Err(AgentError::Tool(ref message))
        if message.contains("unsupported in child agents") && message.contains("child-session routing")));
    backend.shutdown();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Drain only after the writer has stopped: even delayed request/close events
    // would be observed, without a timing-dependent negative assertion.
    assert!(
        events.recv().await.is_none(),
        "child emitted an external event"
    );
}

#[tokio::test]
async fn root_external_tool_still_emits_and_settles_with_structured_content() {
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (writer, task, _) = start_rpc_writer(ExternalEventCapture(tx), Default::default());
    let backend = ExternalTools::detached(writer);
    let tool = external_test_tool(backend.clone());
    let raw = json!({"content":[{"type":"text","text":"answer"}],"structuredContent":{"x":2},"_meta":{"private":true}});
    let run = tool.execute(
        "root-call",
        json!({"childAgent":false}),
        CancellationToken::new(),
        Arc::new(|_| {}),
    );
    let reply = async {
        let request = events.recv().await.unwrap();
        assert_eq!(request["type"], "external_tool_request");
        assert_eq!(request["toolName"], "external_test");
        assert_eq!(request["arguments"], json!({"childAgent":false}));
        assert!(backend
            .respond(&json!({"requestId":request["requestId"],"result":raw}))
            .unwrap());
        request["requestId"].clone()
    };
    let (result, id) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(run, reply) })
            .await
            .unwrap();
    let result = result.unwrap();
    assert_eq!(
        result.content,
        vec![
            TextContentOrImage::text("answer"),
            TextContentOrImage::text(r#"{"x":2}"#)
        ]
    );
    assert_eq!(result.details, json!({"externalToolResult":raw}));
    backend.shutdown();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        json!({"type":"external_tool_request_closed","requestId":id})
    );
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn root_external_tool_cancellation_still_closes_and_retires_request() {
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (writer, task, _) = start_rpc_writer(ExternalEventCapture(tx), Default::default());
    let backend = ExternalTools::detached(writer);
    let tool = external_test_tool(backend.clone());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        tool.execute("pre-cancelled", json!({}), cancelled, Arc::new(|_| {}))
            .await,
        Err(AgentError::Abort)
    ));
    let cancel = CancellationToken::new();
    let run = tool.execute("root-call", json!({}), cancel.clone(), Arc::new(|_| {}));
    let cancellation = async {
        let request = events.recv().await.unwrap();
        assert_eq!(request["type"], "external_tool_request");
        cancel.cancel();
        request["requestId"].clone()
    };
    let (result, id) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(run, cancellation)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(AgentError::Abort)));
    assert!(!backend
        .respond(&json!({"requestId":id,"result":{"content":[]}}))
        .unwrap());
    backend.shutdown();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        json!({"type":"external_tool_request_closed","requestId":id})
    );
    assert!(events.recv().await.is_none());
}
