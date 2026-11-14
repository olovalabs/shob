use super::resource_lifecycle::{rejected, session_files};
use super::*;
async fn launch(root: &Path, args: &[&str], label: &str) -> Host {
    let error = root.join(format!("{label}.stderr"));
    let mut cmd = command(root);
    cmd.args(args);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&error).unwrap())
        .spawn()
        .unwrap();
    let mut host = Host {
        error,
        input: child.stdin.take().unwrap(),
        output: BufReader::new(child.stdout.take().unwrap()).lines(),
        child,
    };
    host.until(|v| v["type"] == "ready").await;
    host
}
async fn fork(root: &Path, config: Option<&str>, source: &str, label: &str) -> Host {
    let mut args = vec!["--fork", source];
    if let Some(config) = config {
        args.extend(["--resource-config", config]);
    }
    launch(root, &args, label).await
}
#[tokio::test]
async fn legacy_native_fork_retains_nonmessage_leaf_without_resource_opt_in() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    super::resource_lifecycle::fixture_model_config(root);
    let mut source = launch(root, &["--session-id", "legacy-source"], "legacy-source").await;
    assert_eq!(
        source
            .request(
                "append_entry",
                json!({"customType":"ordinary","data":{"marker":"legacy custom leaf preserved"}})
            )
            .await["success"],
        true
    );
    source.stop().await;
    let before = session_files(root);
    let mut destination = fork(root, None, "legacy-source", "legacy-fork").await;
    let state = destination.request("get_state", json!({})).await;
    let id = state["data"]["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(id, "legacy-source");
    assert_eq!(
        destination
            .request("get_resource_state", json!({"version":1}))
            .await["success"],
        false
    );
    let branch = destination
        .request("get_session_branch", json!({"customTypes":["ordinary"]}))
        .await;
    assert!(branch.to_string().contains("legacy custom leaf preserved"));
    destination.stop().await;
    preserved(&before, root);
    let before = session_files(root);
    let mut reopened = launch(root, &["--session-id", &id], "legacy-reopen").await;
    assert_eq!(
        reopened
            .request("get_resource_state", json!({"version":1}))
            .await["success"],
        false
    );
    assert!(reopened
        .request("get_session_branch", json!({"customTypes":["ordinary"]}))
        .await
        .to_string()
        .contains("legacy custom leaf preserved"));
    reopened.stop().await;
    assert_eq!(session_files(root), before);
}
fn preserved(before: &std::collections::BTreeMap<PathBuf, Vec<u8>>, root: &Path) {
    let after = session_files(root);
    for (path, bytes) in before {
        assert_eq!(
            after.get(path),
            Some(bytes),
            "fork/reopen mutated existing source {}",
            path.display()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_native_fork_clone_and_old_message_keep_current_package_without_source_operations(
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    put(root.join("agent/models.json"),json!({"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("http://{}/v1",listener.local_addr().unwrap()),"apiKey":"loopback-only","models":[{"id":"test","contextWindow":131072,"maxTokens":256}]}}}).to_string());
    put(
        root.join("agent/settings.json"),
        r#"{"retry":{"enabled":false}}"#,
    );
    for name in ["a", "b"] {
        put(
            root.join(format!(".rpi/packages/{name}/persistent.md")),
            format!("FORK_PACKAGE_{name}"),
        );
    }
    put(root.join(".rpi/packages.json"),json!({"packages":[{"name":"a","version":"1","source":"fixture-a","cargo_package":"none","artifacts":[]},{"name":"b","version":"2","source":"fixture-b","cargo_package":"none","artifacts":[]}]}).to_string());
    let selected = |name: &str| json!({"version":1,"main_package":{"name":name,"instructions":["persistent.md"]},"tools":{"mode":"none"}});
    put(root.join("a.json"), selected("a").to_string());
    put(root.join("b.json"), selected("b").to_string());
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = body(&mut socket).await;
            assert!(request["messages"].to_string().contains("FORK_PACKAGE_a"));
            let frame = json!({"choices":[{"index":0,"delta":{"content":format!("native old answer {index}")},"finish_reason":"stop"}]});
            let payload = format!("data: {frame}\n\ndata: [DONE]\n\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).as_bytes()).await.unwrap();
        }
    });
    let mut host = Host::start(root, "a.json", "fork-source").await;
    host.prompt("native old first user").await;
    host.prompt("native old second user").await;
    let messages = host.request("get_messages", json!({"limit":128})).await;
    let old = messages["data"]["messages"][1]["entryId"]
        .as_str()
        .unwrap()
        .to_owned();
    let message_ids = messages["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["entryId"].clone())
        .collect::<Vec<_>>();
    let initial = host.state().await;
    host.stop().await;
    server.await.unwrap();
    let change = json!({"version":1,"operation_id":"parent-change-a-b","expected":{"revision":initial["binding"]["revision"],"fingerprint":initial["binding"]["fingerprint"]},"target":selected("b")});
    put(root.join("change.json"), change.to_string());
    let mut host =
        Host::start_with_flag(root, "--resource-transition", "change.json", "fork-source").await;
    let current = host.state().await;
    assert_eq!(current["binding"]["revision"], 2);
    assert_eq!(current["binding"]["operationId"], "parent-change-a-b");
    host.stop().await;
    let source_files = session_files(root);
    let source_path = source_files
        .keys()
        .find(|path| path.to_string_lossy().ends_with("_fork-source.jsonl"))
        .unwrap();
    let source_bytes = std::fs::read(source_path).unwrap();
    let source_records = String::from_utf8(source_bytes.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let latest = source_records
        .iter()
        .rev()
        .find(|v| v["kind"] == "entry")
        .unwrap();
    assert_eq!(latest["type"], "custom");
    assert_eq!(latest["data"]["value"]["operationId"], "parent-change-a-b");
    for historical in [false, true] {
        let before = session_files(root);
        let mut fork = fork(
            root,
            Some("b.json"),
            "fork-source",
            if historical { "historical" } else { "clone" },
        )
        .await;
        let state = fork.state().await;
        let id = state["sessionId"].as_str().unwrap().to_owned();
        assert_ne!(id, "fork-source");
        assert_ne!(state["hostId"], current["hostId"]);
        assert_eq!(state["binding"]["revision"], 1);
        assert_eq!(state["binding"]["operationId"], Value::Null);
        assert_eq!(
            state["binding"]["fingerprint"],
            current["binding"]["fingerprint"]
        );
        assert_eq!(state["selection"]["main_package"]["name"], "b");
        let history = fork.request("get_messages", json!({"limit":128})).await;
        assert_eq!(
            history["data"]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["entryId"].clone())
                .collect::<Vec<_>>(),
            message_ids
        );
        if historical {
            let result = fork.request("navigate", json!({"targetId":old})).await;
            assert_eq!(result["success"], true, "{result}");
            assert_eq!(fork.state().await["binding"], state["binding"]);
        }
        let history = fork.request("get_messages", json!({"limit":128})).await;
        assert!(history.to_string().contains("native old first user"));
        if historical {
            assert!(!history.to_string().contains("native old second user"));
        }
        fork.stop().await;
        preserved(&before, root);
        assert_eq!(std::fs::read(source_path).unwrap(), source_bytes);
        let destination = session_files(root)
            .into_keys()
            .find(|path| path.to_string_lossy().ends_with(&format!("_{id}.jsonl")))
            .unwrap();
        let rows = std::fs::read_to_string(&destination)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[0]["parentSessionId"], "fork-source");
        let binding = rows
            .iter()
            .rev()
            .find(|v| v["data"]["key"] == "rpi.resource-selection.v1")
            .unwrap();
        assert_eq!(binding["data"]["value"]["revision"], 1);
        assert!(binding["data"]["value"].get("operations").is_none());
        assert!(binding["data"]["value"].get("operationId").is_none());
        let before = session_files(root);
        let mut reopened = Host::start(root, "b.json", &id).await;
        assert_eq!(reopened.state().await["binding"], state["binding"]);
        let restored_history = reopened.request("get_messages", json!({"limit":128})).await;
        assert!(restored_history
            .to_string()
            .contains("native old first user"));
        reopened.stop().await;
        assert_eq!(session_files(root), before);
        rejected(
            root,
            &["--resource-config", "a.json", "--session-id", &id],
            "selection differs",
        )
        .await;
        // The parent's textual operation ID starts a NEW chain here, rather
        // than being treated as a copied retry or a superseded operation.
        put(root.join("child-change.json"),json!({"version":1,"operation_id":"parent-change-a-b","expected":{"revision":1,"fingerprint":state["binding"]["fingerprint"]},"target":selected("a")}).to_string());
        let mut changed =
            Host::start_with_flag(root, "--resource-transition", "child-change.json", &id).await;
        let child_state = changed.state().await;
        assert_eq!(child_state["binding"]["revision"], 2);
        assert_eq!(child_state["binding"]["operationId"], "parent-change-a-b");
        assert_eq!(child_state["selection"]["main_package"]["name"], "a");
        changed.stop().await;
        let mut reopened = Host::start(root, "a.json", &id).await;
        assert_eq!(reopened.state().await["binding"], child_state["binding"]);
        reopened.stop().await;
        preserved(&source_files, root);
    }
    preserved(&source_files, root);
}
