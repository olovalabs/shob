//! End-to-end smoke test for the `--connect` client against a fake
//! `rpi --server` endpoint.
//!
//! Exercises the real client path (TCP connect → `start_session` → `subscribe`
//! → `send` → event pump → transcript folding) without needing a live agent.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpListener;

use rpi_cli::remote::client::RemoteClient;
use rpi_cli::remote::protocol::RemoteCommand;
use rpi_cli::remote::session::{RemoteSession, TranscriptItem};

async fn write_line(writer: &mut OwnedWriteHalf, value: &Value) {
    let mut line = serde_json::to_string(value).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

async fn respond(writer: &mut OwnedWriteHalf, id: Value, result: Value) {
    write_line(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
    .await;
}

async fn notify(writer: &mut OwnedWriteHalf, sub_id: u64, event: Value) {
    write_line(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": {"subscriptionId": sub_id, "event": event}
        }),
    )
    .await;
}

/// Canned `--mode rpc` lines for a single "ping" → "pong" run.
fn canned_run() -> Vec<Value> {
    vec![
        json!({"type": "agent_start"}),
        json!({"type": "message_start", "message": {"kind": "assistant", "content": []}}),
        json!({
            "type": "message_update",
            "message": {"kind": "assistant", "content": []},
            "assistantMessageEvent": {"type": "text_delta"},
            "eventType": "text_delta",
            "contentIndex": 0,
            "delta": "po"
        }),
        json!({
            "type": "message_update",
            "message": {"kind": "assistant", "content": []},
            "assistantMessageEvent": {"type": "text_delta"},
            "eventType": "text_delta",
            "contentIndex": 0,
            "delta": "ng"
        }),
        json!({
            "type": "message_end",
            "message": {"kind": "assistant", "content": [{"type": "text", "text": "pong"}]}
        }),
        json!({"type": "agent_end", "messageCount": 1, "messages": []}),
    ]
}

async fn fake_server(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut sub_id: u64 = 0;

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        match value.get("method").and_then(Value::as_str).unwrap_or("") {
            "start_session" => {
                respond(&mut writer, id, json!({"sessionId": "session-1"})).await;
            }
            "subscribe" => {
                sub_id += 1;
                respond(
                    &mut writer,
                    id,
                    json!({"subscriptionId": sub_id, "sessionId": "session-1", "status": "subscribed"}),
                )
                .await;
                // The child's readiness line arrives as a forwarded event.
                notify(&mut writer, sub_id, json!({"type": "ready"})).await;
            }
            "send" => {
                respond(
                    &mut writer,
                    id,
                    json!({"sessionId": "session-1", "status": "sent"}),
                )
                .await;
                for event in canned_run() {
                    notify(&mut writer, sub_id, event).await;
                }
            }
            "stop_session" => {
                respond(
                    &mut writer,
                    id,
                    json!({"sessionId": "session-1", "status": "stopped"}),
                )
                .await;
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn client_handshakes_and_folds_a_remote_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(fake_server(listener));

    let mut client = RemoteClient::connect(&addr.to_string()).await.unwrap();

    let started = client.call("start_session", json!({})).await.unwrap();
    let session_id = started["sessionId"].as_str().unwrap().to_string();

    let subscribed = client
        .call("subscribe", json!({"sessionId": session_id}))
        .await
        .unwrap();
    let subscription_id = subscribed["subscriptionId"].as_u64().unwrap();
    let mut events = client.start_event_pump(subscription_id);

    let mut session = RemoteSession::new();
    session.on_user_prompt("ping");
    client
        .send_command(
            &session_id,
            RemoteCommand::Prompt {
                content: "ping".into(),
            }
            .with_id(7),
        )
        .await
        .unwrap();

    let mut saw_terminal = false;
    while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
        session.apply_line(&line);
        if line.get("type").and_then(Value::as_str) == Some("agent_end") {
            saw_terminal = true;
            break;
        }
    }

    assert!(saw_terminal, "never received agent_end");
    assert!(session.ready, "server readiness line was not observed");
    assert!(!session.streaming);
    assert_eq!(session.transcript.items.len(), 2);
    assert_eq!(
        session.transcript.items[0],
        TranscriptItem::User("ping".into())
    );
    assert_eq!(
        session.transcript.items[1],
        TranscriptItem::Assistant("pong".into())
    );

    client.stop_session(&session_id).await;
}

#[tokio::test]
async fn native_driver_receives_answers_and_cancels_remote_approvals() {
    use rpi_cli::session_driver::{DriverCommand, DriverEvent, RemoteDriver, SessionDriver};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (answers_tx, mut answers_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut writer) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let value: Value = serde_json::from_str(&line).unwrap();
            let id = value["id"].clone();
            match value["method"].as_str().unwrap() {
                "start_session" => respond(&mut writer, id, json!({"sessionId":"s"})).await,
                "subscribe" => {
                    respond(&mut writer, id, json!({"subscriptionId":9})).await;
                    notify(&mut writer, 9, json!({"type":"ready"})).await;
                }
                "send" => {
                    respond(&mut writer, id, json!({"status":"sent"})).await;
                    let command = &value["params"]["command"];
                    match command["type"].as_str().unwrap() {
                        "prompt" => {
                            notify(&mut writer, 9, json!({"type":"agent_start"})).await;
                            notify(&mut writer, 9, json!({"type":"ui_dialog_request", "requestId":"q1",
                                "toolCallId":"c1", "ui":{"question":"Allow?", "options":["Yes","No"]}})).await;
                        }
                        "ui_dialog_response" => {
                            answers_tx.send(command.clone()).unwrap();
                            notify(&mut writer, 9, json!({"type":"ui_dialog_closed", "requestId":command["requestId"]})).await;
                            if command["requestId"] == "q1" {
                                notify(&mut writer, 9, json!({"type":"ui_dialog_request", "requestId":"q2",
                                    "toolCallId":"c2", "ui":{"question":"Reason?", "kind":"input"}})).await;
                            } else {
                                notify(
                                    &mut writer,
                                    9,
                                    json!({"type":"agent_end","messageCount":0,"messages":[]}),
                                )
                                .await;
                            }
                        }
                        "get_state" => {}
                        other => panic!("unexpected command {other}"),
                    }
                }
                "stop_session" => {
                    respond(&mut writer, id, json!({"status":"stopped"})).await;
                    break;
                }
                other => panic!("unexpected method {other}"),
            }
        }
    });
    let driver = RemoteDriver::connect(&addr.to_string(), None)
        .await
        .unwrap();
    let mut events = driver.events().unwrap();
    driver
        .send(DriverCommand::Prompt("test".into()))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let DriverEvent::UiDialog(request) = events.recv().await.unwrap() {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(first.request_id, "q1");
    assert_eq!(first.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(first.ui["options"][1], "No");
    driver
        .send(DriverCommand::UiDialogResponse {
            request_id: first.request_id,
            answer: Some(json!({"values":["No"],"kind":"selector"})),
        })
        .await
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(3), answers_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["requestId"], "q1");
    assert_eq!(answer["answer"]["values"][0], "No");
    assert_eq!(answer["cancelled"], false);
    let second = tokio::time::timeout(Duration::from_secs(3), async {
        let mut closed_first = false;
        loop {
            match events.recv().await.unwrap() {
                DriverEvent::UiDialogClosed(id) if id == "q1" => closed_first = true,
                DriverEvent::UiDialog(request) => {
                    assert!(closed_first);
                    break request;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(second.request_id, "q2");
    driver
        .send(DriverCommand::UiDialogResponse {
            request_id: second.request_id,
            answer: None,
        })
        .await
        .unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(3), answers_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled["requestId"], "q2");
    assert_eq!(cancelled["cancelled"], true);
    assert!(cancelled.get("answer").is_none());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(events.recv().await.unwrap(), DriverEvent::UiDialogClosed(id) if id == "q2")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    driver.shutdown().await;
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

/// The pump must ignore non-event frames (responses) rather than confuse them
/// with events.
#[tokio::test]
async fn pump_ignores_request_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(fake_server(listener));

    let mut client = RemoteClient::connect(&addr.to_string()).await.unwrap();
    let started = client.call("start_session", json!({})).await.unwrap();
    let session_id = started["sessionId"].as_str().unwrap().to_string();
    let subscribed = client
        .call("subscribe", json!({"sessionId": session_id}))
        .await
        .unwrap();
    let sub_id = subscribed["subscriptionId"].as_u64().unwrap();
    let mut events = client.start_event_pump(sub_id);

    // The `send` response ("sent") is a response frame, not an event.
    client
        .send_command(&session_id, RemoteCommand::Ping.with_id(1))
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    // The first forwarded frame is the readiness event, never the response.
    assert_eq!(first["type"], "ready");
}

/// A fake server that requires the token `s3cret` before any other request.
async fn auth_server(listener: TcpListener, token: &'static str) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let (read_half, mut writer) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let mut authenticated = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                if !authenticated {
                    if method == "authenticate" {
                        let provided = value
                            .get("params")
                            .and_then(|p| p.get("token"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if provided == token {
                            authenticated = true;
                            respond(&mut writer, id, json!({"authenticated": true})).await;
                        } else {
                            write_line(
                                &mut writer,
                                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "invalid token"}}),
                            )
                            .await;
                            break;
                        }
                    } else {
                        write_line(
                            &mut writer,
                            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "authentication required"}}),
                        )
                        .await;
                    }
                    continue;
                }
                if method == "start_session" {
                    respond(&mut writer, id, json!({"sessionId": "session-1"})).await;
                }
            }
        });
    }
}

#[tokio::test]
async fn token_authentication_is_enforced() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(auth_server(listener, "s3cret"));

    // 1) No token at all → other methods are refused.
    {
        let mut client = RemoteClient::connect(&addr).await.unwrap();
        assert!(client.call("start_session", json!({})).await.is_err());
    }

    // 2) Wrong token → rejected.
    {
        let mut client = RemoteClient::connect(&addr).await.unwrap();
        assert!(client.authenticate("nope").await.is_err());
    }

    // 3) Correct token → allowed.
    {
        let mut client = RemoteClient::connect(&addr).await.unwrap();
        client.authenticate("s3cret").await.unwrap();
        let started = client.call("start_session", json!({})).await.unwrap();
        assert_eq!(started["sessionId"], "session-1");
    }
}

/// Real end-to-end check: drives the *actual* client against a live
/// `rpi --server`. Opt-in via `RPI_REMOTE_E2E_ADDR` (e.g. `127.0.0.1:9899`) so
/// the default test run stays hermetic.
#[tokio::test]
async fn real_server_roundtrip_if_configured() {
    let Ok(addr) = std::env::var("RPI_REMOTE_E2E_ADDR") else {
        return;
    };
    let prompt = std::env::var("RPI_REMOTE_E2E_PROMPT")
        .unwrap_or_else(|_| "Reply with exactly the single word: PONG".to_string());

    let mut client = RemoteClient::connect(&addr).await.unwrap();
    let token = std::env::var("RPI_REMOTE_E2E_TOKEN").unwrap_or_default();
    client
        .authenticate(&token)
        .await
        .expect("authentication failed (set RPI_REMOTE_E2E_TOKEN)");
    let started = client.call("start_session", json!({})).await.unwrap();
    let session_id = started["sessionId"].as_str().unwrap().to_string();
    let subscribed = client
        .call("subscribe", json!({"sessionId": session_id}))
        .await
        .unwrap();
    let sub_id = subscribed["subscriptionId"].as_u64().unwrap();
    let mut events = client.start_event_pump(sub_id);

    let mut session = RemoteSession::new();
    session.on_user_prompt(prompt.clone());
    client
        .send_command(
            &session_id,
            RemoteCommand::Prompt { content: prompt }.with_id(1),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(line)) => {
                session.apply_line(&line);
                if line.get("type").and_then(Value::as_str) == Some("agent_end") {
                    break;
                }
            }
            _ => break,
        }
    }

    println!(
        "--- remote transcript ---\n{}",
        session.transcript.to_text()
    );
    assert!(session.ready, "server readiness line was never observed");
    assert!(!session.streaming, "run never reached agent_end");
    let assistant: String = session
        .transcript
        .items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        !assistant.trim().is_empty(),
        "the remote agent produced no assistant text"
    );
    client.stop_session(&session_id).await;
}
