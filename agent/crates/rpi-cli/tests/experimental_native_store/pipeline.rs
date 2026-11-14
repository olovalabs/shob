use super::*;
use rpi_harness::session::{experimental_native::ExperimentalNativeRepo, types::*};
fn models(f: &Fixture, address: std::net::SocketAddr) {
    std::fs::write(f.agent.join("models.json"),json!({"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("http://{address}/v1"),"apiKey":"fixture-only","models":[{"id":"fixture","contextWindow":32768,"maxTokens":256}]}}}).to_string()).unwrap();
}
async fn respond(socket: &mut tokio::net::TcpStream, frame: &str) {
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{frame}",frame.len()).as_bytes()).await.unwrap();
}
fn cli(f: &Fixture, mode: &str) -> tokio::process::Command {
    let mut command = f.command();
    command.args([
        "--offline",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--provider=fixture",
        "--model=fixture",
        "--print",
        "--mode",
        mode,
        "--experimental-native-store=native",
    ]);
    command
}
#[tokio::test]
async fn tool_pipeline_uses_native_history_and_preserves_legacy_sentinels() {
    let f = Fixture::new();
    let before = f.sentinels();
    std::fs::write(
        f.temp.path().join("fixture.txt"),
        "tool result from native fixture",
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    models(&f, listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let body = request(&mut first).await;
        assert!(body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "read"));
        let delta = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"fixture-tool","type":"function","function":{"name":"read","arguments":"{\"path\":\"fixture.txt\"}"}}]},"finish_reason":"tool_calls"}]});
        respond(&mut first, &format!("data: {delta}\n\ndata: [DONE]\n\n")).await;
        let (mut second, _) = listener.accept().await.unwrap();
        let body = request(&mut second).await;
        assert!(body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("tool result from native fixture"))));
        respond(&mut second,"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"tool saved.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
    });
    let result =
        output(cli(&f, "json").args(["--tools=read", "--approve", "read the fixture"])).await;
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    tokio::time::timeout(Duration::from_secs(3), provider)
        .await
        .unwrap()
        .unwrap();
    let repo = ExperimentalNativeRepo::new(f.native.clone()).await.unwrap();
    let metadata = repo.list().await.unwrap().remove(0);
    let storage = repo.open(&metadata).await.unwrap();
    let entries = storage.find_entries(&EntryQuery::default()).await.unwrap();
    assert!(entries.iter().any(|entry|matches!(entry,Entry::Message(m)if matches!(&m.message,rpi_agent::AgentMessage::ToolResult(t)if t.tool_call_id=="fixture-tool"))));
    assert_eq!(entries.iter().filter(|e|matches!(e,Entry::Message(m)if matches!(m.message,rpi_agent::AgentMessage::Assistant(_)))).count(),2);
    assert!(storage
        .find_open_operations("main", None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(f.sentinels(), before);
}
#[tokio::test]
async fn text_and_json_report_nonzero_when_native_assistant_persistence_fails() {
    for mode in ["text", "json"] {
        let f = Fixture::new();
        let before = f.sentinels();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        models(&f, listener.local_addr().unwrap());
        let native = f.native.clone();
        let provider = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            request(&mut socket).await;
            // The user request has committed before the first provider call.
            // Inject an isolated storage failure before returning the assistant.
            let path = std::fs::read_dir(native)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|s| s == "rpinative"))
                .unwrap();
            let db = rusqlite::Connection::open(path).unwrap();
            db.busy_timeout(Duration::from_secs(3)).unwrap();
            db.execute_batch("CREATE TRIGGER refuse_assistant BEFORE INSERT ON facts WHEN NEW.category='entry' BEGIN SELECT RAISE(ABORT,'injected native persistence failure'); END;").unwrap();
            drop(db);
            respond(&mut socket,"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"not durably saved\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
        });
        let result = output(cli(&f, mode).args(["--no-builtin-tools", "failure fixture"])).await;
        assert!(
            !result.status.success(),
            "false ACK: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("persist")
                || String::from_utf8_lossy(&result.stdout).contains("failed")
        );
        tokio::time::timeout(Duration::from_secs(3), provider)
            .await
            .unwrap()
            .unwrap();
        let repo = ExperimentalNativeRepo::new(f.native.clone()).await.unwrap();
        let metadata = repo.list().await.unwrap().remove(0);
        let storage = repo.open(&metadata).await.unwrap();
        let entries = storage.find_entries(&EntryQuery::default()).await.unwrap();
        assert!(!entries.iter().any(|e|matches!(e,Entry::Message(m)if matches!(m.message,rpi_agent::AgentMessage::Assistant(_)))));
        assert_eq!(f.sentinels(), before);
    }
}
