//! Real CLI boundary tests. All config/session/provider data is synthetic and
//! confined to temporary directories; no credentials or user sessions are read.
use serde_json::json;
#[cfg(feature = "experimental-native-store")]
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
#[cfg(feature = "experimental-native-store")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    temp: tempfile::TempDir,
    agent: PathBuf,
    old: PathBuf,
    native: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let old = temp.path().join("old sessions");
        let native = temp.path().join("native");
        std::fs::create_dir_all(&agent).unwrap();
        for root in [
            &old,
            &temp.path().join(".rpi/sessions"),
            &temp.path().join("settings sessions"),
        ] {
            std::fs::create_dir_all(root).unwrap();
            // A valid legacy v3 artifact, deliberately not opened/upgraded.
            std::fs::write(root.join("legacy.jsonl"),format!("{}\n",json!({"type":"session","version":3,"id":"legacy","timestamp":"2026-01-01T00:00:00.000Z","cwd":temp.path()}))).unwrap();
        }
        std::fs::write(
            agent.join("settings.json"),
            json!({"sessionDir":temp.path().join("settings sessions")}).to_string(),
        )
        .unwrap();
        Self {
            temp,
            agent,
            old,
            native,
        }
    }
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
        command
            .current_dir(self.temp.path())
            .env("RPI_CODING_AGENT_DIR", &self.agent)
            .env("RPI_CODING_AGENT_SESSION_DIR", &self.old)
            .env("HOME", self.temp.path())
            .env("USERPROFILE", self.temp.path())
            .env_remove("RPI_FORCE_TUI")
            .env_remove("RPI_EXTENSIONS_DIR")
            .env_remove("RPI_SESSION_ID")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
    fn sentinels(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, out);
                } else {
                    out.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        for root in [
            &self.old,
            &self.temp.path().join(".rpi/sessions"),
            &self.temp.path().join("settings sessions"),
        ] {
            visit(self.temp.path(), root, &mut out);
        }
        out
    }
}
async fn output(command: &mut tokio::process::Command) -> Output {
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("CLI timed out")
        .unwrap()
}
#[tokio::test]
async fn illegal_optins_reject_before_early_dispatch_or_legacy_writes() {
    let f = Fixture::new();
    let before = f.sentinels();
    let cases = vec![
        vec!["auth"],
        vec!["events"],
        vec!["update"],
        vec!["install"],
        vec!["extension", "list"],
        vec!["extension", "--help"],
        vec!["extension", "update", "--help"],
        vec!["extension", "update"],
        vec!["resource", "list", "--scope", "project"],
        vec!["resource", "inspect", "skill", "demo", "--scope", "project"],
        vec!["resource", "recover", "--scope", "project"],
        vec!["package"],
        vec!["dev"],
        vec!["--connect=127.0.0.1:1"],
        vec!["--print", "--continue"],
        vec!["--print", "--resume"],
        vec!["--print", "--session=legacy"],
        vec!["--print", "--session-id=legacy"],
        vec!["--print", "--fork=legacy"],
        vec!["--print", "--no-session"],
        vec!["--print", "--session-dir=old sessions"],
        vec!["--print", "--server=false"],
        vec!["--print", "--web"],
        vec!["--print", "--mode=rpc"],
        vec![
            "--print",
            "--export=old sessions/legacy.jsonl",
            "export.html",
        ],
        vec!["--print", "--list-models"],
        vec!["--mode=json"],
    ];
    for args in cases {
        let result = output(
            f.command()
                .args(&args)
                .arg("--experimental-native-store")
                .arg(&f.native),
        )
        .await;
        assert_eq!(
            result.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("experimental-native-store"));
        if args.first() == Some(&"extension") {
            assert!(String::from_utf8_lossy(&result.stderr).contains("not a subcommand"));
        }
        assert!(!f.native.exists());
        assert!(!f.temp.path().join("export.html").exists());
        assert!(!f.temp.path().join(".rpi/.resource-lease").exists());
        assert!(!f.agent.join(".resource-lease").exists());
        assert_eq!(f.sentinels(), before);
    }
    let result = output(
        f.command()
            .args(["--print", "--experimental-native-store=native"])
            .env("RPI_FORCE_TUI", "1"),
    )
    .await;
    assert_eq!(result.status.code(), Some(2));
    assert!(!f.native.exists());
    assert_eq!(f.sentinels(), before);
}
#[cfg(not(feature = "experimental-native-store"))]
#[tokio::test]
async fn feature_off_reserved_flag_never_falls_back_to_jsonl() {
    let f = Fixture::new();
    let before = f.sentinels();
    for args in [
        vec!["--print", "--experimental-native-store=native"],
        vec!["--experimental-native-store", "native", "--print"],
    ] {
        let result = output(f.command().args(args)).await;
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("Cargo feature"));
        assert!(!f.native.exists());
        assert_eq!(f.sentinels(), before);
    }
}
#[cfg(feature = "experimental-native-store")]
async fn request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header = loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let length: usize = String::from_utf8_lossy(&bytes[..header])
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < header + length {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&bytes[header..header + length]).unwrap()
}
#[cfg(feature = "experimental-native-store")]
#[tokio::test]
async fn real_text_and_json_prints_create_only_fresh_native_artifacts_and_persist_facts() {
    use rpi_harness::session::{experimental_native::ExperimentalNativeRepo, types::*};
    for mode in ["text", "json"] {
        let f = Fixture::new();
        let before = f.sentinels();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        std::fs::write(f.agent.join("models.json"),json!({"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("http://{address}/v1"),"apiKey":"fixture-only","models":[{"id":"fixture","contextWindow":32768,"maxTokens":256}]}}}).to_string()).unwrap();
        let provider = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = request(&mut socket).await;
                assert!(body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("synthetic durable prompt"))));
                let frame="data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"durably saved.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{frame}",frame.len()).as_bytes()).await.unwrap();
            }
        });
        for iteration in 0..2 {
            let result = output(f.command().args([
                "--offline",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
                "--no-builtin-tools",
                "--provider=fixture",
                "--model=fixture",
                "--print",
                "--mode",
                mode,
                "--name=fixture name",
                "--experimental-native-store=native",
                "synthetic durable prompt",
            ]))
            .await;
            assert!(
                result.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("durably saved."));
            if mode == "json" {
                let lines = String::from_utf8_lossy(&result.stdout)
                    .lines()
                    .map(|l| serde_json::from_str::<Value>(l).unwrap())
                    .collect::<Vec<_>>();
                assert!(lines
                    .iter()
                    .any(|l| l["type"] == "result" && l["outcome"] == "completed"));
            }
            let repo = ExperimentalNativeRepo::new(f.native.clone()).await.unwrap();
            let sessions = repo.list().await.unwrap();
            assert_eq!(sessions.len(), iteration + 1);
            for metadata in sessions {
                let storage = repo.open(&metadata).await.unwrap();
                assert_eq!(
                    storage.get_name().await.unwrap().as_deref(),
                    Some("fixture name")
                );
                let entries = storage
                    .find_entries(&EntryQuery {
                        entry_type: Some("message"),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                assert_eq!(entries.len(), 2);
                assert!(entries.iter().any(|e|matches!(e,Entry::Message(m)if matches!(m.message,rpi_agent::AgentMessage::User(_)))));
                assert!(entries.iter().any(|e|matches!(e,Entry::Message(m)if matches!(m.message,rpi_agent::AgentMessage::Assistant(_)))));
                assert!(storage
                    .find_open_operations("main", None)
                    .await
                    .unwrap()
                    .is_empty());
                assert!(storage.history_revision().await.unwrap() >= 4);
            }
            assert_eq!(f.sentinels(), before);
            assert!(std::fs::read_dir(&f.native).unwrap().all(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "rpinative")));
        }
        tokio::time::timeout(Duration::from_secs(3), provider)
            .await
            .unwrap()
            .unwrap();
    }
}
#[path = "experimental_native_store/guards.rs"]
mod guards;
#[cfg(feature = "experimental-native-store")]
#[path = "experimental_native_store/pipeline.rs"]
mod pipeline;
#[cfg(feature = "experimental-native-store")]
#[path = "experimental_native_store/resources.rs"]
mod resources;
