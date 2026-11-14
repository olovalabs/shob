//! Exercise settings/CLI precedence, real SSE stalls and harness retry offline.
use std::{process::Stdio, time::Duration};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

async fn run_case(settings_idle: u64, cli_idle: Option<u64>, stall: bool) {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        agent.join("settings.json"),
        json!({
            "streamIdleTimeout": settings_idle,
            "retry": { "enabled": true, "maxRetries": 1, "baseDelayMs": 0 }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({
            "providers": { "fixture": {
                "api": "openai-completions", "baseUrl": format!("http://{address}/v1"),
                "apiKey": "fixture-only", "models": [{ "id": "reasoner", "reasoning": true,
                "contextWindow": 131072, "maxTokens": 256, "compat": {
                        "thinkingFormat": "deepseek",
                        "requiresReasoningContentOnAssistantMessages": true,
                        "unknownFixtureFlag": true
                    }
                }]
            }}
        })
        .to_string(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let mut requests = 0;
        let mut stalled_socket = None;
        for attempt in 0..if stall { 2 } else { 1 } {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = request_body(&mut socket).await;
            assert_eq!(request["thinking"]["type"], "enabled");
            requests += 1;
            if stall && attempt == 0 {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
                socket
                    .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                    .await
                    .unwrap();
                stalled_socket = Some(socket);
                continue;
            }
            let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Recovered.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", frame.len()).as_bytes()).await.unwrap();
            if !stall {
                // Longer than the settings timeout: CLI zero must disable it.
                tokio::time::sleep(Duration::from_millis(1250)).await;
            }
            socket.write_all(frame.as_bytes()).await.unwrap();
        }
        drop(stalled_socket);
        requests
    });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
    command
        .current_dir(root.path())
        .env("RPI_CODING_AGENT_DIR", &agent)
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .args([
            "--offline",
            "--no-session",
            "--package-only",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--no-builtin-tools",
            "--provider",
            "fixture",
            "--model",
            "reasoner",
            "--thinking",
            "high",
            "--print",
            "--timeout",
            "10",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(seconds) = cli_idle {
        command.arg(format!("--stream-idle-timeout={seconds}"));
    }
    let out_path = root.path().join("stdout.log");
    let err_path = root.path().join("stderr.log");
    let mut child = command
        .arg("hello")
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
    if status.is_err() {
        child.kill().await.unwrap();
    }
    let stderr = std::fs::read_to_string(&err_path).unwrap();
    let stdout = std::fs::read_to_string(&out_path).unwrap();
    let status = status
        .unwrap_or_else(|_| {
            panic!("idle timeout did not recover; stdout: {stdout}\nstderr: {stderr}")
        })
        .unwrap();
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Recovered."), "{stdout}\n{stderr}");
    assert_eq!(
        stderr
            .matches("unknown field \"unknownFixtureFlag\"")
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(server.await.unwrap(), if stall { 2 } else { 1 });
}

#[tokio::test]
async fn model_listing_reports_compat_warnings_once() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    std::fs::write(agent.join("models.json"), json!({"providers":{"fixture":{
        "api":"openai-completions", "models":[{"id":"test", "compat":{"unknownListingFlag":true}}]
    }}}).to_string()).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
        .current_dir(root.path())
        .env("RPI_CODING_AGENT_DIR", agent)
        .args([
            "--offline",
            "--package-only",
            "--no-extensions",
            "--list-models",
            "fixture",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("test"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .matches("unknown field \"unknownListingFlag\"")
            .count(),
        1,
        "{stderr}"
    );
}

#[tokio::test]
async fn stream_idle_settings_and_cli_recover_stalls_and_allow_disabling() {
    run_case(1, None, true).await;
    run_case(0, Some(1), true).await;
    run_case(1, Some(0), false).await;
}
