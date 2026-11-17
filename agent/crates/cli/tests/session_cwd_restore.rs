//! End-to-end: restoring a session returns the agent to the directory the
//! session was created in.
//!
//! The launch path adopts the session's recorded cwd before any resource load,
//! so the observable contract lands in the provider request itself: the default
//! system prompt embeds `Current working directory: …`, and a run resumed from
//! a different directory must present the recorded directory there — not the
//! launch directory the shell happened to be in.
use std::sync::{Arc, Mutex};
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

/// A one-request fixture provider: captures the request body, answers with a
/// plain stop frame.
async fn serve_one_request(listener: Arc<tokio::net::TcpListener>, bodies: Arc<Mutex<Vec<Value>>>) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let body = request_body(&mut socket).await;
    let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                frame.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.write_all(frame.as_bytes()).await.unwrap();
    bodies.lock().unwrap().push(body);
}

struct Fixture {
    root: tempfile::TempDir,
    agent: std::path::PathBuf,
    sessions: std::path::PathBuf,
    listener: Arc<tokio::net::TcpListener>,
}

async fn start_fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let sessions = root.path().join("sessions");
    let project = root.path().join("session-cwd-project");
    let launch = root.path().join("session-cwd-launch");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&launch).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({
            "providers": { "fixture": {
                "api": "openai-completions", "baseUrl": format!("http://{address}/v1"),
                "apiKey": "fixture-only", "models": [{ "id": "reasoner",
                "contextWindow": 131072, "maxTokens": 256 }]
            }}
        })
        .to_string(),
    )
    .unwrap();
    Fixture {
        root,
        agent,
        sessions,
        listener: Arc::new(listener),
    }
}

/// One print-mode run of the real binary.
async fn run_rpi(
    fixture: &Fixture,
    cwd: &std::path::Path,
    extra_args: &[&str],
    prompt: &str,
) -> (std::process::ExitStatus, String, String) {
    let out_path = cwd.join("stdout.log");
    let err_path = cwd.join("stderr.log");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
    command
        .current_dir(cwd)
        .env("RPI_CODING_AGENT_DIR", &fixture.agent)
        .env("RPI_CODING_AGENT_SESSION_DIR", &fixture.sessions)
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .args([
            "--offline",
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
            "--print",
            "--timeout",
            "15",
        ])
        .args(extra_args)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    if status.is_err() {
        panic!("run did not finish in time (cwd {})", cwd.display());
    }
    let status = status.unwrap().unwrap();
    let stdout = std::fs::read_to_string(&out_path).unwrap();
    let stderr = std::fs::read_to_string(&err_path).unwrap();
    (status, stdout, stderr)
}

/// A session created in `project-a` and resumed (`-c`) from a different
/// directory must run in `project-a`: the provider request's system prompt
/// names the recorded directory, not the launch directory. Before the adoption
/// existed, the same launch presented the launch directory — the bug.
#[tokio::test]
async fn resuming_from_another_directory_runs_in_the_recorded_one() {
    let fixture = start_fixture().await;
    let project = fixture.root.path().join("session-cwd-project");
    let launch = fixture.root.path().join("session-cwd-launch");

    // Run 1 creates the session in the shared session root, recording the
    // project directory as its cwd.
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &[], "first").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(bodies.lock().unwrap().len(), 1);

    // Run 2 resumes the latest session from a different directory.
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &launch, &["--continue"], "second").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("ok."), "{stdout}\n{stderr}");

    // Adopting another project's directory switches tools, context files, and
    // extensions to that project; the user has to see it happen.
    assert!(
        stderr.contains("where it was created"),
        "the adoption must be announced: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("session-cwd-project") && stderr.contains("session-cwd-launch"),
        "the notice must name both directories: {stderr}"
    );

    let requests = bodies.lock().unwrap();
    assert_eq!(requests.len(), 1, "stderr: {stderr}");
    let request_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        request_text.contains("session-cwd-project"),
        "the resumed run must present the session's recorded directory: {request_text}"
    );
    assert!(
        !request_text.contains("session-cwd-launch"),
        "the launch directory must not leak into the resumed run: {request_text}"
    );
}

/// A session whose recorded directory has since been deleted falls back to the
/// launch cwd, and says so on the run's stderr (headless modes have no
/// transcript to put a note in).
#[tokio::test]
async fn a_missing_recorded_directory_falls_back_and_says_so() {
    let fixture = start_fixture().await;
    let project = fixture.root.path().join("session-cwd-project");
    let launch = fixture.root.path().join("session-cwd-launch");

    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &[], "first").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");

    // The recorded directory disappears before the resume.
    std::fs::remove_dir_all(&project).unwrap();

    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &launch, &["--continue"], "second").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("no longer exists"),
        "the fallback must be announced: {stdout}\n{stderr}"
    );

    let requests = bodies.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        request_text.contains("session-cwd-launch"),
        "the run must fall back to the launch directory: {request_text}"
    );
}
