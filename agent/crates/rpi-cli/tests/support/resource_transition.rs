use super::resource_lifecycle::{fixture_model_config, rejected, session_files};
use super::*;

fn transition(binding: &Value, operation: &str, target: Value) -> Value {
    json!({"version":1,"operation_id":operation,"expected":{"revision":binding["revision"],"fingerprint":binding["fingerprint"]},"target":target})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controlled_same_id_restart_changes_actual_model_resources_and_preserves_history() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    put(root.join("agent/models.json"), json!({"providers":{"fixture":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1",listener.local_addr().unwrap()),
        "apiKey":"fixture-only","models":[{"id":"test","contextWindow":131072,"maxTokens":256}]
    }}}).to_string());
    put(
        root.join("agent/settings.json"),
        r#"{"defaultTools":["docs"],"retry":{"enabled":false}}"#,
    );
    for name in ["a", "b"] {
        put(
            root.join(format!("{name}.md")),
            format!("TRANSITION_INSTRUCTION_{name}"),
        );
        skill(
            root.join(format!("{name}/SKILL.md")),
            name,
            &format!("TRANSITION_SKILL_{name}"),
        );
    }
    let a = json!({"version":1,"instructions":["a.md"],"skills":["a/SKILL.md"],"extensions":[fixture.library],"tools":{"mode":"only","names":["read","echo"]}});
    let mut b = a.clone();
    b["instructions"] = json!(["b.md"]);
    b["skills"] = json!(["b/SKILL.md"]);
    b["tools"] = json!({"mode":"default"});
    put(root.join("a.json"), a.to_string());
    put(root.join("b.json"), b.to_string());
    let server = tokio::spawn(async move {
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = body(&mut socket).await;
            let system = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "system" || m["role"] == "developer")
                .map(|m| m["content"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            let (name, other) = if index < 2 { ("a", "b") } else { ("b", "a") };
            assert!(
                system.contains(&format!("TRANSITION_INSTRUCTION_{name}")),
                "{system}"
            );
            assert!(
                system.contains(&format!("TRANSITION_SKILL_{name}")),
                "{system}"
            );
            assert!(!system.contains(&format!("TRANSITION_INSTRUCTION_{other}")));
            assert!(!system.contains(&format!("TRANSITION_SKILL_{other}")));
            let tools: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect();
            assert!(tools.contains(&"read") && tools.contains(&"echo"));
            assert_eq!(tools.contains(&"bash"), name == "b");
            if name == "b" {
                assert!(
                    request["messages"].to_string().contains("echo: ABI_a"),
                    "{request}"
                );
                assert!(request["messages"].to_string().contains("run-transition-a"));
            }
            let frame = if index % 2 == 0 {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":format!("echo-{name}"),"type":"function","function":{"name":"echo","arguments":format!("{{\"text\":\"ABI_{name}\"}}")}}]},"finish_reason":"tool_calls"}]})
            } else {
                assert!(request["messages"]
                    .to_string()
                    .contains(&format!("echo: ABI_{name}")));
                json!({"choices":[{"index":0,"delta":{"content":"transition complete"},"finish_reason":"stop"}]})
            };
            let response = format!("data: {frame}\n\ndata: [DONE]\n\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
        }
    });
    let mut host = Host::start(root, "a.json", "same-id").await;
    let initial = host.state().await;
    assert_eq!(initial["binding"]["revision"], 1);
    host.prompt("run-transition-a").await;
    host.stop().await;
    rejected(
        root,
        &["--resource-config", "b.json", "--session-id", "same-id"],
        "selection differs",
    )
    .await;
    let request = transition(&initial["binding"], "change-a-b", b);
    put(root.join("transition.json"), request.to_string());
    let mut host =
        Host::start_with_flag(root, "--resource-transition", "transition.json", "same-id").await;
    let applied = host.state().await;
    assert_eq!(applied["sessionId"], initial["sessionId"]);
    assert_eq!(applied["binding"]["revision"], 2);
    assert_eq!(applied["binding"]["operationId"], "change-a-b");
    assert_ne!(
        applied["binding"]["fingerprint"],
        initial["binding"]["fingerprint"]
    );
    host.prompt("run-transition-b").await;
    host.stop().await;
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let before = session_files(root);
    let mut host =
        Host::start_with_flag(root, "--resource-transition", "transition.json", "same-id").await;
    assert_eq!(host.state().await["binding"], applied["binding"]);
    let history = host.request("get_messages", json!({})).await.to_string();
    assert!(history.contains("echo: ABI_a") && history.contains("echo: ABI_b"));
    host.stop().await;
    assert_eq!(
        session_files(root),
        before,
        "identical retry must not append"
    );
    let mut host = Host::start(root, "b.json", "same-id").await;
    assert_eq!(host.state().await["binding"], applied["binding"]);
    host.stop().await;
    fixture.assert_registrations(&root.join("registrations.log"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_skill_preflight_and_committed_retry_precede_registration_and_writer() {
    let fixture = Fixture::build().await;
    for source in ["explicit-file", "explicit-root", "package"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture_model_config(root);
        put(
            root.join("none.json"),
            r#"{"version":1,"tools":{"mode":"none"}}"#,
        );
        let mut host = Host::start(root, "none.json", "skill-transition").await;
        let initial = host.state().await;
        host.stop().await;
        let path = if source == "package" {
            root.join(".rpi/packages/probe/skills/review/SKILL.md")
        } else {
            root.join("skills/review/SKILL.md")
        };
        let mut target =
            json!({"version":1,"extensions":[fixture.library],"tools":{"mode":"none"}});
        if source == "package" {
            target["main_package"] = json!({"name":"probe"});
            put(
                root.join(".rpi/packages.json"),
                json!({"packages":[{
                    "name":"probe","version":"1","source":"fixture",
                    "cargo_package":"none","artifacts":[]
                }]})
                .to_string(),
            );
        } else {
            target["skills"] = if source == "explicit-file" {
                json!(["skills/review/SKILL.md"])
            } else {
                json!(["skills"])
            };
        }
        put(
            root.join("transition.json"),
            transition(&initial["binding"], "skill-target", target).to_string(),
        );
        let args = [
            "--resource-transition",
            "transition.json",
            "--session-id",
            "skill-transition",
        ];
        let log = root.join("registrations.log");
        let transcript = session_files(root)
            .into_keys()
            .find(|p| {
                p.extension().is_some_and(|e| e == "jsonl")
                    && !p.to_string_lossy().ends_with(".progress.jsonl")
            })
            .unwrap();
        let original = std::fs::read(&transcript).unwrap();
        for malformed in [
            "---\nname: [\n---\nMalformed YAML\n",
            "---\nname: review\n---\nMissing description\n",
            "---\nname: wrong-name\ndescription: Valid description\n---\nInvalid metadata\n",
        ] {
            put(&path, malformed);
            rejected(root, &args, "selected skill").await;
            assert!(!log.exists(), "{source}: malformed target registered code");
        }
        // Even an unrecoverable transition transcript must not get as far as
        // writer preparation (or mask the earlier selected-Skill diagnostic).
        let mut torn = original.clone();
        torn.extend_from_slice(b"{torn transition Skill tail");
        put(&transcript, torn);
        rejected(root, &args, "selected skill").await;
        assert!(!log.exists(), "{source}: malformed target registered code");
        put(&transcript, &original);

        // A new operation validates its target, not the previous empty inventory.
        skill(&path, "review", "COMMITTED_TARGET");
        let original_skill = std::fs::read(&path).unwrap();
        let mut host = Host::start_with_flag(
            root,
            "--resource-transition",
            "transition.json",
            "skill-transition",
        )
        .await;
        let applied = host.state().await;
        assert_eq!(applied["binding"]["revision"], 2);
        assert_eq!(applied["binding"]["operationId"], "skill-target");
        assert_eq!(applied["sessionId"], initial["sessionId"]);
        assert_eq!(applied["loadedSkills"][0]["name"], "review");
        host.stop().await;
        fixture.assert_registrations(&log);
        put(&log, "");
        // The same operation ID does not authorize changed valid source bytes.
        // Keep metadata/declaration identical and change only the Skill body.
        put(
            &path,
            String::from_utf8(original_skill.clone())
                .unwrap()
                .replace("Skill body COMMITTED_TARGET", "Changed valid target body"),
        );
        rejected(root, &args, "Skill inventory changed").await;
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "",
            "{source}: changed committed target registered code"
        );
        // Restoring the committed bytes makes the identical retry a no-append
        // success, preserving the revision and operation association.
        put(&path, original_skill);
        let before = session_files(root);
        let mut host = Host::start_with_flag(
            root,
            "--resource-transition",
            "transition.json",
            "skill-transition",
        )
        .await;
        assert_eq!(host.state().await["binding"], applied["binding"]);
        host.stop().await;
        assert_eq!(session_files(root), before, "{source}: retry appended");
        fixture.assert_registrations(&log);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_conflicts_invalid_targets_and_concurrent_writers_are_read_only() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture_model_config(root);
    let none = json!({"version":1,"tools":{"mode":"none"}});
    put(root.join("none.json"), none.to_string());
    let mut host = Host::start(root, "none.json", "conflicts").await;
    let initial = host.state().await;
    host.stop().await;
    let target = json!({"version":1,"extensions":[fixture.library],"tools":{"mode":"only","names":["echo"]}});
    let request = transition(&initial["binding"], "one", target.clone());
    put(root.join("transition.json"), request.to_string());
    let mut host = Host::start_with_flag(
        root,
        "--resource-transition",
        "transition.json",
        "conflicts",
    )
    .await;
    let applied = host.state().await;
    // A second contender cannot own the writer, even with the identical operation.
    rejected(
        root,
        &[
            "--resource-transition",
            "transition.json",
            "--session-id",
            "conflicts",
        ],
        "already open",
    )
    .await;
    for key in [
        "rpi.resource-selection.v1",
        "rpi.resource-selection.operations",
        "rpi.resource-inventory.v2",
    ] {
        assert_eq!(
            host.request(
                "append_entry",
                json!({"customType":"rpi.value","data":{"key":key,"value":null}})
            )
            .await["success"],
            false
        );
    }
    // Simulate loss of the controller/child immediately after authoritative
    // application; the identical request at the end must reopen without append.
    host.child.kill().await.unwrap();
    drop(host);
    let registration = std::fs::read(root.join("registrations.log")).unwrap();
    let mut stale = transition(&initial["binding"], "stale", target.clone());
    put(root.join("bad.json"), stale.to_string());
    rejected(
        root,
        &[
            "--resource-transition",
            "bad.json",
            "--session-id",
            "conflicts",
        ],
        "stale",
    )
    .await;
    stale["operation_id"] = json!("one");
    stale["target"] = none.clone();
    put(root.join("bad.json"), stale.to_string());
    rejected(
        root,
        &[
            "--resource-transition",
            "bad.json",
            "--session-id",
            "conflicts",
        ],
        "operation conflict",
    )
    .await;
    assert_eq!(
        std::fs::read(root.join("registrations.log")).unwrap(),
        registration,
        "conflicts must not register code"
    );
    let invalid_dll = format!("invalid.{}", std::env::consts::DLL_EXTENSION);
    put(root.join(&invalid_dll), "not a native library");
    for invalid in [
        json!({"version":2,"tools":{"mode":"none"}}),
        json!({"version":1,"skills":["missing/SKILL.md"],"tools":{"mode":"none"}}),
        json!({"version":1,"tools":{"mode":"only","names":["unknown"]}}),
        json!({"version":1,"extensions":[invalid_dll],"tools":{"mode":"none"}}),
    ] {
        put(
            root.join("bad.json"),
            transition(&applied["binding"], "invalid", invalid).to_string(),
        );
        rejected(
            root,
            &[
                "--resource-transition",
                "bad.json",
                "--session-id",
                "conflicts",
            ],
            "",
        )
        .await;
    }
    for bad in ["{".to_string(), " ".repeat(65537), json!({"version":1,"operation_id":"","expected":{"revision":0,"fingerprint":null},"target":none}).to_string()] {
        put(root.join("bad.json"),bad);
        rejected(root,&["--resource-transition","bad.json","--session-id","conflicts"],"").await;
    }
    let mut host = Host::start_with_flag(
        root,
        "--resource-transition",
        "transition.json",
        "conflicts",
    )
    .await;
    assert_eq!(host.state().await["binding"], applied["binding"]);
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pure_capability_negotiation_never_starts_resources_or_reads_credentials() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for base in [root.join(".rpi"), root.join("agent")] {
        put(base.join("packages.json"), "{malformed registry");
        put(base.join("resource-receipts.json"), "{malformed receipts");
        put(base.join("resource-transaction.json"), "{malformed journal");
        put(base.join("skills/bad/SKILL.md"), "---\nname: [invalid\n---");
        let dll = base
            .join("extensions")
            .join(fixture.library.file_name().unwrap());
        std::fs::create_dir_all(dll.parent().unwrap()).unwrap();
        std::fs::copy(&fixture.library, &dll).unwrap();
        put(
            base.join(format!(
                "extensions/malformed.{}",
                std::env::consts::DLL_EXTENSION
            )),
            "not a DLL",
        );
    }
    // A provider startup would resolve executable credential configuration.
    // This task-owned sentinel must never be written by help/profile probing.
    put(
        root.join("agent/models.json"),
        json!({"providers":{"fixture":{
            "api":"openai-completions","apiKey":"!echo forbidden > credential-read.marker",
            "baseUrl":"http://127.0.0.1:1","models":[{"id":"test"}]
        }}})
        .to_string(),
    );
    put(root.join("agent/auth.json"), "{malformed credentials");
    put(root.join("agent/settings.json"), "{malformed settings");
    put(
        root.join("sessions/history.jsonl"),
        "{malformed session metadata",
    );
    fn snapshot(
        root: &Path,
    ) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        fn walk(
            path: &Path,
            result: &mut std::collections::BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)>,
        ) {
            if path.is_file() {
                result.insert(
                    path.to_path_buf(),
                    (
                        std::fs::read(path).unwrap(),
                        std::fs::metadata(path).unwrap().modified().unwrap(),
                    ),
                );
            } else {
                for entry in std::fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), result);
                }
            }
        }
        let mut result = std::collections::BTreeMap::new();
        walk(root, &mut result);
        result
    }
    let before = snapshot(root);
    // Windows sharing exclusion makes any credential-file read fail, not merely
    // parsing. The query/help must succeed without touching the locked file.
    #[cfg(windows)]
    let credentials_guard = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(root.join("agent/auth.json"))
            .unwrap()
    };
    for args in [
        vec!["--help"],
        vec!["--resource-capabilities"],
        vec![
            "--resource-capabilities",
            "resource",
            "recover",
            "--scope",
            "project",
        ],
        vec!["--resource-capabilities=invalid"],
    ] {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(root)
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .env("RPI_CODING_AGENT_SESSION_DIR", root.join("sessions"))
            .env("RPI_EXTENSIONS_DIR", root.join(".rpi/extensions"))
            .env(
                "RPI_PLUGIN_STUB_REGISTRATION_LOG",
                root.join("registrations.log"),
            )
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let _keep_stdin_open = child.stdin.take().unwrap();
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("resourceState") && !text.contains("credential-read.marker"));
        if args == ["--help"] {
            assert!(output.status.success());
            assert!(text.contains("--resource-capabilities"));
        } else {
            assert!(text.len() <= 16384);
            let profile: Value = serde_json::from_str(&text).unwrap();
            if args.len() == 1 && args[0] == "--resource-capabilities" {
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(profile["version"], 1);
                assert_eq!(profile["exactSelection"]["versions"], json!([1]));
                assert_eq!(
                    profile["management"]["kinds"],
                    json!(["package", "skill", "extension"])
                );
                assert_eq!(
                    profile["management"]["scopes"],
                    json!(["project", "global"])
                );
                assert_eq!(
                    profile["management"]["operations"],
                    json!([
                        "list",
                        "inspect",
                        "install",
                        "update",
                        "reinstall",
                        "remove",
                        "recover"
                    ])
                );
                assert_eq!(profile["transition"]["version"], 1);
                assert_eq!(profile["transition"]["maxRequestBytes"], 65536);
                assert_eq!(profile["loadedFacts"], false);
            } else {
                assert!(!output.status.success());
                assert_eq!(profile["success"], false);
            }
        }
    }
    #[cfg(windows)]
    drop(credentials_guard);
    assert_eq!(
        snapshot(root),
        before,
        "help/query must not mutate metadata, create leases, execute credentials, or register DLLs"
    );
}
