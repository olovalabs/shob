//! Nonexecuting process regressions; all inputs/output roots owned by each test.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
fn put(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}
fn input(root: &Path) -> PathBuf {
    let path = root.join("sessions/saved.jsonl");
    let header = json!({"kind":"header","version":4,"id":"saved-id","createdAt":0,"cwd":root,"progressFormat":"assistant-progress-v1"});
    let message = |id: &str, seq: u64, parent: Value, text: &str| json!({"kind":"entry","lane":"main","type":"message","id":id,"seq":seq,"timestamp":0,"parentId":parent,"message":{"kind":"user","role":"user","content":text,"timestamp":0}});
    let lines = [
        header,
        message("one", 1, Value::Null, "kept first"),
        message("abandoned", 2, json!("one"), "abandoned branch"),
        json!({"kind":"lane","lane":"main","leafId":"one","seq":3}),
        message("two", 4, json!("one"), "kept second"),
        message("three", 5, json!("two"), "kept third"),
    ];
    put(
        &path,
        lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    );
    put(PathBuf::from(format!("{}.progress.jsonl",path.display())),json!({"format":"assistant-progress-v1","session_id":"saved-id","state":{"sequence":0,"frames":[]}}).to_string()+"\n");
    put(
        PathBuf::from(format!("{}.lock", path.display())),
        "existing native writer lock",
    );
    path
}
fn request(root: &Path, path: &Path) -> Value {
    json!({"version":1,"sessionId":"saved-id","sessionRoot":root.join("sessions"),"sessionPath":path,"limit":1,"customTypes":[],"cursor":null})
}
async fn invoke(root: &Path, request: &Value) -> Value {
    put(root.join("request.json"), request.to_string());
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(root)
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .env("RPI_CODING_AGENT_SESSION_DIR", root.join("sessions"))
            .arg("--resource-session-read")
            .arg(root.join("request.json"))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.stdout.len() <= 8 * 1024 * 1024);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
#[tokio::test]
async fn saved_reader_large_cold_rows_use_bounded_ranges_without_false_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let path = input(root);
    let text = "x".repeat(1200 * 1024);
    let custom = json!({"kind":"entry","lane":"main","type":"custom","id":"large-custom","seq":6,"timestamp":0,"parentId":"three","customType":"fixture.journal","data":text});
    let message = json!({"kind":"entry","lane":"main","type":"message","id":"large-message","seq":7,"timestamp":0,"parentId":"large-custom","message":{"kind":"user","role":"user","content":text,"timestamp":0}});
    let original = std::fs::read(&path).unwrap();
    let bytes = [original, format!("{custom}\n{message}\n").into_bytes()].concat();
    put(&path, &bytes);
    let mut query = request(root, &path);
    query["limit"] = json!(128);
    query["customTypes"] = json!(["fixture.journal"]);
    let response = invoke(root, &query).await;
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(
        response["data"]["messages"][3]["content"].as_str(),
        Some(text.as_str())
    );
    assert_eq!(
        response["data"]["customEntries"][0]["data"].as_str(),
        Some(text.as_str())
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[tokio::test]
async fn saved_reader_canonical_branch_cursor_and_no_settings_startup() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    for name in ["settings.json", "models.json", "auth.json", "trust.json"] {
        put(
            root.join("agent").join(name),
            "BROKEN JSON must not be loaded",
        );
    }
    let mut query = request(root, &path);
    let one = invoke(root, &query).await;
    assert_eq!(one["success"], true, "{one}");
    assert_eq!(one["data"]["leafId"], "three");
    assert_eq!(one["data"]["messages"][0]["entryId"], "one");
    assert_eq!(
        one["data"]["pending"],
        json!({"executionQueueOrRecovery":false,"progress":false,"idleProofSupported":true})
    );
    let mut forged = query.clone();
    forged["cursor"] = one["data"]["cursor"].clone();
    // Sequence 2 exists, but is not a message on the canonical branch.
    forged["cursor"]["afterSeq"] = json!(2);
    let invalid = invoke(root, &forged).await;
    assert_eq!(invalid["error"]["code"], "invalid_data", "{invalid}");
    query["cursor"] = one["data"]["cursor"].clone();
    let two = invoke(root, &query).await;
    assert_eq!(two["data"]["messages"][0]["entryId"], "two");
    assert!(!two.to_string().contains("abandoned"));
    query["cursor"] = two["data"]["cursor"].clone();
    let three = invoke(root, &query).await;
    assert_eq!(three["data"]["messages"][0]["entryId"], "three");
    assert!(three["data"]["cursor"].is_null());
    let mut bytes = std::fs::read(&path).unwrap();
    bytes
        .extend_from_slice(b"{\"kind\":\"lane\",\"lane\":\"main\",\"leafId\":\"one\",\"seq\":6}\n");
    put(&path, &bytes);
    query["cursor"] = two["data"]["cursor"].clone();
    let changed = invoke(root, &query).await;
    assert_eq!(changed["error"]["code"], "branch_changed", "{changed}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[tokio::test]
async fn saved_reader_active_writer_missing_lock_and_strict_corruption_never_repair() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let query = request(root, &path);
    let original = std::fs::read(&path).unwrap();
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    assert_eq!(invoke(root, &query).await["error"]["code"], "busy");
    drop(lock);
    std::fs::remove_file(&lock_path).unwrap();
    assert_eq!(invoke(root, &query).await["error"]["code"], "unsupported");
    assert!(!lock_path.exists());
    put(&lock_path, "same observation lock");
    for damaged in [
        original[..original.len() - 1].to_vec(),
        [original.clone(), b"{".to_vec()].concat(),
        [
            original.clone(),
            b"{\"kind\":\"bogus\",\"seq\":6}\n".to_vec(),
        ]
        .concat(),
    ] {
        put(&path, &damaged);
        let error = invoke(root, &query).await;
        assert_eq!(error["success"], false, "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }
    put(&path, &original);
    let progress = PathBuf::from(format!("{}.progress.jsonl", path.display()));
    let mut damaged = std::fs::read(&progress).unwrap();
    damaged.push(b'{');
    put(&progress, &damaged);
    assert_eq!(invoke(root, &query).await["success"], false);
    assert_eq!(std::fs::read(&progress).unwrap(), damaged);
}
#[tokio::test]
async fn saved_reader_pending_work_is_readable_and_identity_custom_privacy_are_strict() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let query = request(root, &path);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(json!({"kind":"record","type":"operation_started","id":"run-pending","seq":6,"timestamp":0,"lane":"main","sourceLeafId":"three","intent":{"kind":"run","originalPrompt":[],"initialMessages":[]}}).to_string().as_bytes());
    bytes.push(b'\n');
    put(&path, &bytes);
    let pending = invoke(root, &query).await;
    assert_eq!(pending["success"], true, "{pending}");
    assert_eq!(pending["data"]["pending"]["executionQueueOrRecovery"], true);
    assert_eq!(pending["data"]["messages"][0]["entryId"], "one");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let mut wrong = query.clone();
    wrong["sessionId"] = json!("wrong-id");
    assert_eq!(invoke(root, &wrong).await["success"], false);
    wrong = query.clone();
    wrong["customTypes"] = json!(["rpi.value"]);
    assert_eq!(invoke(root, &wrong).await["success"], false);
    wrong = query.clone();
    wrong["unexpected"] = json!(true);
    assert_eq!(invoke(root, &wrong).await["success"], false);
    wrong = query.clone();
    wrong["limit"] = json!(129);
    assert_eq!(invoke(root, &wrong).await["success"], false);
}
#[tokio::test]
async fn saved_reader_native_queue_progress_and_legacy_facts_are_readable_not_execution() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let query = request(root, &path);
    let original = std::fs::read(&path).unwrap();
    let queued = json!({"kind":"record","type":"queue_enqueued","id":"queue-1","seq":6,"timestamp":0,"lane":"main","queue":"nextRun","target":{"type":"message","id":"queued-message","message":{"kind":"user","role":"user","content":"pending native queue","timestamp":0}}});
    put(
        &path,
        [original.clone(), format!("{queued}\n").into_bytes()].concat(),
    );
    let view = invoke(root, &query).await;
    assert_eq!(view["success"], true, "{view}");
    assert_eq!(view["data"]["pending"]["executionQueueOrRecovery"], true);
    put(&path, &original);
    let progress = PathBuf::from(format!("{}.progress.jsonl", path.display()));
    let frames=json!({"format":"assistant-progress-v1","session_id":"saved-id","state":{"sequence":1,"frames":[{"id":"frame-1","seq":1,"timestamp":0,"lane":"main","runId":"interrupted","streamIndex":0,"op":"append","frame":{"type":"text_delta","delta":"unfinished stream"}}]}}).to_string()+"\n";
    put(&progress, &frames);
    let view = invoke(root, &query).await;
    assert_eq!(view["success"], true, "{view}");
    assert_eq!(view["data"]["pending"]["progress"], true);
    assert_eq!(std::fs::read_to_string(&progress).unwrap(), frames);
    let mut lines = String::from_utf8(original)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut header: Value = serde_json::from_str(&lines[0]).unwrap();
    header.as_object_mut().unwrap().remove("progressFormat");
    lines[0] = header.to_string();
    put(&path, lines.join("\n") + "\n");
    let view = invoke(root, &query).await;
    assert_eq!(view["success"], true, "{view}");
    assert_eq!(view["data"]["pending"]["idleProofSupported"], false);
    assert_eq!(std::fs::read_to_string(&progress).unwrap(), frames);
}
#[tokio::test]
async fn saved_reader_progress_only_change_invalidates_cursor_but_keeps_blocked_history_visible() {
    use rpi_harness::session::jsonl::JsonlSessionStorage;
    use rpi_harness::session::{DefaultIdGenerator, SystemClock};
    use std::sync::Arc;

    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let mut query = request(root, &path);
    let before = invoke(root, &query).await;
    assert_eq!(before["success"], true, "{before}");
    let history = std::fs::read(&path).unwrap();
    let progress_path = PathBuf::from(format!("{}.progress.jsonl", path.display()));
    let progress = json!({
        "format":"assistant-progress-v1", "session_id":"saved-id",
        "state":{"sequence":1,"frames":[{
            "id":"pending-frame", "seq":1, "timestamp":0, "lane":"main",
            "runId":"pending-run", "streamIndex":0, "op":"append",
            "frame":{"type":"text_delta","delta":"not a committed message"}
        }]}
    })
    .to_string()
        + "\n";
    put(&progress_path, &progress);

    query["cursor"] = before["data"]["cursor"].clone();
    let stale = invoke(root, &query).await;
    assert_eq!(stale["error"]["code"], "branch_changed", "{stale}");
    query["cursor"] = Value::Null;
    let blocked = invoke(root, &query).await;
    assert_eq!(blocked["success"], true, "{blocked}");
    assert_eq!(blocked["data"]["leafId"], before["data"]["leafId"]);
    assert_ne!(blocked["data"]["source"], before["data"]["source"]);
    assert_eq!(blocked["data"]["messages"], before["data"]["messages"]);
    assert_eq!(blocked["data"]["pending"]["progress"], true);
    assert_eq!(blocked["data"]["pending"]["idleProofSupported"], true);
    query["cursor"] = blocked["data"]["cursor"].clone();
    let next = invoke(root, &query).await;
    assert_eq!(next["success"], true, "{next}");
    assert_eq!(next["data"]["messages"][0]["entryId"], "two");
    assert_eq!(next["data"]["pending"]["progress"], true);

    // Readable history and supported idle-proof machinery are not idle permission.
    // This is the strict loader used by transition preflight, not a normal open.
    let result = JsonlSessionStorage::load_for_resource_transition(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(root.to_path_buf())),
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await;
    assert!(matches!(result, Err(error) if error.to_string().contains("pending native progress")));
    assert_eq!(std::fs::read(&path).unwrap(), history);
    assert_eq!(std::fs::read_to_string(&progress_path).unwrap(), progress);
}

#[tokio::test]
async fn saved_reader_message_and_custom_serialized_limits_are_not_empty_success() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let mut query = request(root, &path);
    let original = std::fs::read(&path).unwrap();
    let custom = json!({"kind":"entry","lane":"main","type":"custom","id":"custom-big","seq":6,"timestamp":0,"parentId":"three","customType":"fixture.journal","data":"x".repeat(2*1024*1024)});
    put(
        &path,
        [original.clone(), format!("{custom}\n").into_bytes()].concat(),
    );
    query["customTypes"] = json!(["fixture.journal"]);
    assert_eq!(
        invoke(root, &query).await["error"]["code"],
        "limit_exceeded"
    );
    let message = json!({"kind":"entry","lane":"main","type":"message","id":"message-big","seq":6,"timestamp":0,"parentId":"three","message":{"kind":"user","role":"user","content":"x".repeat(6*1024*1024),"timestamp":0}});
    put(
        &path,
        [original, format!("{message}\n").into_bytes()].concat(),
    );
    query["customTypes"] = json!([]);
    query["limit"] = json!(128);
    let before = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        invoke(root, &query).await["error"]["code"],
        "limit_exceeded"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
}
#[tokio::test]
async fn saved_reader_explicit_bounds_preserve_large_inputs_and_refuse_escaped_sources() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let path = input(root);
    let query = request(root, &path);
    let progress = PathBuf::from(format!("{}.progress.jsonl", path.display()));
    let old = std::fs::read(&progress).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&progress)
        .unwrap()
        .set_len(65 * 1024 * 1024)
        .unwrap();
    assert_eq!(
        invoke(root, &query).await["error"]["code"],
        "limit_exceeded"
    );
    assert_eq!(
        std::fs::metadata(&progress).unwrap().len(),
        65 * 1024 * 1024
    );
    put(&progress, &old);
    let mut wrong = query.clone();
    wrong["sessionRoot"] = json!(root.join("agent"));
    std::fs::create_dir_all(root.join("agent")).unwrap();
    assert_eq!(invoke(root, &wrong).await["success"], false);
    let mut bytes = std::fs::read(&path).unwrap();
    for seq in 6..=65542 {
        bytes.extend_from_slice(
            format!("{{\"kind\":\"fact\",\"fact\":\"name\",\"name\":null,\"seq\":{seq}}}\n")
                .as_bytes(),
        );
    }
    put(&path, &bytes);
    let overflow = invoke(root, &query).await;
    assert_eq!(overflow["error"]["code"], "limit_exceeded", "{overflow}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
