//! Stage B real CLI acceptance, reusing the owned current-SDK fixture and RPC driver.
use super::*;
async fn cli(root: &Path, args: &[&str]) -> (bool, Value) {
    let out = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(root)
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .env(
                "RPI_PLUGIN_STUB_REGISTRATION_LOG",
                root.join("registrations.log"),
            )
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        out.stdout.len() <= 256 * 1024,
        "unbounded management output"
    );
    let value=serde_json::from_slice(&out.stdout).unwrap_or_else(|_|json!({"text":String::from_utf8_lossy(&out.stdout),"error":String::from_utf8_lossy(&out.stderr)}));
    (out.status.success(), value)
}
async fn manage(
    root: &Path,
    op: &str,
    kind: &str,
    name: &str,
    scope: &str,
    source: Option<&Path>,
) -> Value {
    let path = source.map(|p| p.to_str().unwrap());
    let mut args = vec!["resource", op, kind, name, "--scope", scope];
    if let Some(p) = path {
        args.extend(["--path", p]);
    }
    let (ok, value) = cli(root, &args).await;
    assert!(ok, "{args:?}: {value}");
    assert_eq!(value["success"], true, "{value}");
    value["data"].clone()
}
fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(path: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        if path.is_file() {
            out.insert(path.to_owned(), std::fs::read(path).unwrap());
        } else if path.is_dir() {
            for e in std::fs::read_dir(path).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    walk(&root.join(".rpi"), &mut result);
    walk(&root.join("agent"), &mut result);
    result
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_packages_scopes_snapshots_actual_model_tools_and_host_leases() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let a = root.join("source-a");
    let b = root.join("source-b");
    let extra = root.join("source-skill");
    let library = root.join(format!("source-native.{}", std::env::consts::DLL_EXTENSION));
    std::fs::copy(&fixture.library, &library).unwrap();
    for (source, label) in [(&a, "A"), (&b, "B")] {
        skill(
            source.join("skills/review/SKILL.md"),
            "review",
            &format!("INSTALLED_{label}_SKILL"),
        );
        put(
            source.join("skills/review/references/data.txt"),
            format!("{label} references"),
        );
        put(
            source.join("prompts/persistent.md"),
            format!("INSTALLED_{label}_INSTRUCTION"),
        );
        put(
            source.join("prompts/template.md"),
            "NEVER_IMPLICIT_TEMPLATE",
        );
    }
    put(
        a.join("Cargo.toml"),
        "[package]\nname='must-not-build'\nversion='0.0.1'\nbuild='build.rs'\n",
    );
    put(
        a.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, \"built\").unwrap(); }}",
            root.join("BUILD_RAN").to_str().unwrap()
        ),
    );
    put(
        a.join("extensions/helper")
            .with_extension(std::env::consts::DLL_EXTENSION),
        std::fs::read(&fixture.library).unwrap(),
    );
    skill(extra.join("SKILL.md"), "extra", "STANDALONE_INSTALLED");
    put(extra.join("references/keep.txt"), "reference snapshot");
    manage(root, "install", "package", "demo", "global", Some(&a)).await;
    manage(root, "install", "package", "demo", "project", Some(&b)).await;
    for scope in ["global", "project"] {
        manage(root, "install", "skill", "extra", "project", Some(&extra)).await; // repeated install is idempotent
        if scope == "global" {
            manage(root, "install", "skill", "extra", "global", Some(&extra)).await;
        }
        manage(
            root,
            "install",
            "extension",
            "standalone",
            scope,
            Some(&library),
        )
        .await;
    }
    let unchanged = manage(root, "install", "package", "demo", "global", Some(&a)).await;
    assert_eq!(unchanged["outcome"], "unchanged");
    let before = snapshot(root);
    for scope in ["project", "global"] {
        let (ok, listed) = cli(root, &["resource", "list", "--scope", scope]).await;
        assert!(ok, "{listed}");
        assert_eq!(listed["data"]["resources"].as_array().unwrap().len(), 3);
        let inspected = manage(root, "inspect", "package", "demo", scope, None).await;
        assert_eq!(inspected["resources"][0]["version"], Value::Null);
        assert_eq!(inspected["resources"][0]["loaded"], Value::Null);
        assert_eq!(inspected["resources"][0]["inUse"], false);
    }
    assert_eq!(snapshot(root), before);
    assert!(!root.join("BUILD_RAN").exists());
    assert!(!root.join("registrations.log").exists());
    // Native installation is a snapshot, never a symlink to the original bundle.
    for source in [&a, &b, &extra] {
        std::fs::remove_dir_all(source).unwrap();
    }
    std::fs::remove_file(&library).unwrap();
    assert!(root
        .join("agent/packages/demo/prompts/template.md")
        .is_file());
    assert!(root
        .join(".rpi/packages/demo/skills/review/references/data.txt")
        .is_file());
    assert!(root
        .join("agent/skills/extra/references/keep.txt")
        .is_file());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    put(root.join("agent/models.json"),json!({"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("http://{}/v1",listener.local_addr().unwrap()),"apiKey":"fixture-only","models":[{"id":"test","contextWindow":131072,"maxTokens":256}]}}}).to_string());
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = body(&mut socket).await;
            let messages = request["messages"].as_array().unwrap();
            let system = messages
                .iter()
                .filter(|m| m["role"] == "system" || m["role"] == "developer")
                .map(|m| m["content"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            let global = messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s == "installed-global")
            });
            let (wanted, excluded) = if global { ("A", "B") } else { ("B", "A") };
            assert!(
                system.contains(&format!("INSTALLED_{wanted}_INSTRUCTION")),
                "{system}"
            );
            assert!(
                system.contains(&format!("INSTALLED_{wanted}_SKILL")),
                "{system}"
            );
            assert!(
                !system.contains(&format!("INSTALLED_{excluded}_")),
                "{system}"
            );
            assert!(system.contains("STANDALONE_INSTALLED"));
            assert!(!system.contains("NEVER_IMPLICIT_TEMPLATE"));
            let tools = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(tools, ["read", "echo"]);
            let result = messages.iter().find(|m| m["role"] == "tool");
            let frame = if let Some(result) = result {
                assert!(result["content"]
                    .to_string()
                    .contains("echo: INSTALLED_ABI"));
                json!({"choices":[{"index":0,"delta":{"content":"installed success"},"finish_reason":"stop"}]})
            } else {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"installed-echo","type":"function","function":{"name":"echo","arguments":"{\"text\":\"INSTALLED_ABI\"}"}}]},"finish_reason":"tool_calls"}]})
            };
            let text = format!("data: {frame}\n\ndata: [DONE]\n\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len()).as_bytes()).await.unwrap();
        }
    });
    for (filename, scope) in [("global.json", "global"), ("project.json", "project")] {
        let base = if scope == "global" {
            root.join("agent")
        } else {
            root.join(".rpi")
        };
        let mut selection = json!({"version":1,"main_package":{"name":"demo","scope":scope,"instructions":["prompts/persistent.md"]},"skills":[base.join("skills/extra")],"tools":{"mode":"only","names":["read","echo"]}});
        if scope == "project" {
            selection["extensions"] = json!([base
                .join("extensions")
                .join(format!("standalone.{}", std::env::consts::DLL_EXTENSION))]);
        }
        put(root.join(filename), selection.to_string());
    }
    let mut global = Host::start(root, "global.json", "installed-global").await;
    let mut project = Host::start(root, "project.json", "installed-project").await;
    assert!(
        PathBuf::from(global.state().await["package"]["root"].as_str().unwrap())
            .starts_with(std::fs::canonicalize(root.join("agent")).unwrap())
    );
    tokio::join!(
        global.prompt("installed-global"),
        project.prompt("installed-project")
    );
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    fixture.assert_registrations(&root.join("registrations.log"));
    for scope in ["global", "project"] {
        let (ok, state) = cli(root, &["resource", "list", "--scope", scope]).await;
        assert!(ok, "{state}");
        assert!(state["data"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["inUse"] == true && r["loaded"].is_null()));
        let (ok, value) = cli(
            root,
            &["resource", "remove", "package", "demo", "--scope", scope],
        )
        .await;
        assert!(!ok && value["error"].as_str().unwrap().contains("in use"));
    }
    for args in [
        vec!["package", "unlink", "demo"],
        vec!["uninstall", "standalone"],
        vec!["package", "link", "demo", "--path", "missing", "--force"],
        vec!["extension", "update", "demo"],
    ] {
        assert!(!cli(root, &args).await.0);
    }
    global.stop().await;
    assert!(
        !cli(
            root,
            &["resource", "remove", "skill", "extra", "--scope", "global"]
        )
        .await
        .0
    );
    project.child.kill().await.unwrap();
    project.child.wait().await.unwrap();
    drop(project);
    let mut reopened = Host::start(root, "project.json", "installed-project").await;
    assert_eq!(reopened.state().await["sessionId"], "installed-project");
    reopened.stop().await;
    // Resource-owned Packages do not break legacy extension update-all/alias.
    assert!(cli(root, &["extension", "update"]).await.0);
    assert!(cli(root, &["package", "update"]).await.0);
    assert!(!cli(root, &["package", "unlink", "demo"]).await.0);
    assert!(!cli(root, &["uninstall", "standalone"]).await.0);
    let replacement = root.join("new-source");
    skill(replacement.join("SKILL.md"), "extra", "UPDATED_STANDALONE");
    for scope in ["project", "global"] {
        assert_eq!(
            manage(root, "update", "skill", "extra", scope, Some(&replacement)).await["outcome"],
            "replaced"
        );
        assert_eq!(
            manage(
                root,
                "reinstall",
                "skill",
                "extra",
                scope,
                Some(&replacement)
            )
            .await["outcome"],
            "replaced"
        );
    }
    let reject = command(root)
        .args([
            "--resource-config",
            "project.json",
            "--session-id",
            "installed-project",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!reject.status.success());
    assert!(String::from_utf8_lossy(&reject.stderr).contains("inventory changed"));
    for scope in ["project", "global"] {
        for kind in ["package", "skill", "extension"] {
            let name = match kind {
                "package" => "demo",
                "skill" => "extra",
                _ => "standalone",
            };
            assert_eq!(
                manage(root, "remove", kind, name, scope, None).await["outcome"],
                "removed"
            );
            assert_eq!(
                manage(root, "remove", kind, name, scope, None).await["outcome"],
                "absent"
            );
        }
    }
}

#[tokio::test]
async fn readonly_catalog_unmanaged_candidates_corruption_and_concurrent_requests() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for scope in ["project", "global"] {
        let (ok, v) = cli(root, &["resource", "list", "--scope", scope]).await;
        assert!(ok, "{v}");
    }
    assert!(!root.join(".rpi").exists() && !root.join("agent").exists());
    put(
        root.join(".rpi/packages/suspicious/Cargo.toml"),
        "intentionally invalid Cargo metadata",
    );
    put(
        root.join(".rpi/extensions")
            .join(format!("incompatible.{}", std::env::consts::DLL_EXTENSION)),
        "not a library",
    );
    let before = snapshot(root);
    let (ok, v) = cli(root, &["resource", "list", "--scope", "project"]).await;
    assert!(ok, "{v}");
    assert!(v["data"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["ownership"] == "unmanaged"));
    assert_eq!(snapshot(root), before);
    assert!(!root.join(".rpi/.resource-lease").exists());
    assert!(!root.join("registrations.log").exists());
    let source = root.join("source");
    skill(source.join("SKILL.md"), "standalone", "safe");
    put(root.join(".rpi/packages.json"), "{broken");
    let bytes = std::fs::read(root.join(".rpi/packages.json")).unwrap();
    assert!(
        !cli(
            root,
            &[
                "resource",
                "install",
                "skill",
                "safe",
                "--scope",
                "project",
                "--path",
                source.to_str().unwrap()
            ]
        )
        .await
        .0
    );
    assert_eq!(
        bytes,
        std::fs::read(root.join(".rpi/packages.json")).unwrap()
    );
    std::fs::remove_file(root.join(".rpi/packages.json")).unwrap();
    let args = [
        "resource",
        "install",
        "skill",
        "safe",
        "--scope",
        "project",
        "--path",
        source.to_str().unwrap(),
    ];
    let (a, b) = tokio::join!(cli(root, &args), cli(root, &args));
    assert!(a.0 || b.0);
    for result in [&a, &b] {
        if !result.0 {
            assert!(result.1["error"].as_str().unwrap().contains("in use"));
        }
    }
    assert!(
        manage(root, "inspect", "skill", "safe", "project", None).await["resources"][0]
            ["installed"]
            == true
    );
    put(root.join(".rpi/skills/safe/unmanaged.txt"), "keep");
    assert!(
        !cli(
            root,
            &["resource", "remove", "skill", "safe", "--scope", "project"]
        )
        .await
        .0
    );
    assert_eq!(
        std::fs::read(root.join(".rpi/skills/safe/unmanaged.txt")).unwrap(),
        b"keep"
    );
}
