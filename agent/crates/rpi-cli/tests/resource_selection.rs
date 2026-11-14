//! Native process acceptance tests with automatically built isolated cdylibs.
//! No external model, installed extensions, or user resources are used.
#[path = "support/local_resources.rs"]
mod local_resources;
#[path = "support/resource_binding_corruption.rs"]
mod resource_binding_corruption;
#[path = "support/resource_fixture.rs"]
mod resource_fixture;
#[path = "support/resource_fork.rs"]
mod resource_fork;
#[path = "support/resource_lifecycle.rs"]
mod resource_lifecycle;
#[path = "support/resource_transition.rs"]
mod resource_transition;
#[path = "support/saved_resource_read.rs"]
mod saved_resource_read;
use resource_fixture::Fixture;
use serde_json::{json, Value};
use sha2::Digest as _;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

fn put(path: impl AsRef<Path>, text: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn skill(path: impl AsRef<Path>, name: &str, marker: &str) {
    put(
        path,
        format!("---\nname: {name}\ndescription: {marker}\n---\nSkill body {marker}\n"),
    );
}
fn command(root: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
    cmd.current_dir(root)
        .env("RPI_CODING_AGENT_DIR", root.join("agent"))
        .env("RPI_CODING_AGENT_SESSION_DIR", root.join("sessions"))
        .env(
            "RPI_PLUGIN_STUB_REGISTRATION_LOG",
            root.join("registrations.log"),
        )
        .env("RPI_EXTENSIONS_DIR", root.join("agent/extensions"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .args([
            "--offline",
            "--provider",
            "fixture",
            "--model",
            "test",
            "--mode",
            "rpc",
        ])
        .kill_on_drop(true);
    cmd
}
struct Host {
    error: PathBuf,
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}
impl Host {
    async fn start(root: &Path, config: &str, id: &str) -> Self {
        Self::start_with_flag(root, "--resource-config", config, id).await
    }
    async fn start_with_flag(root: &Path, flag: &str, config: &str, id: &str) -> Self {
        let error = std::fs::File::create(root.join(format!("{id}.stderr"))).unwrap();
        let mut child = command(root)
            .args([flag, config, "--session-id", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(error)
            .spawn()
            .unwrap();
        let mut host = Self {
            error: root.join(format!("{id}.stderr")),
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
        };
        let ready = host.until(|v| v["type"] == "ready").await;
        assert_eq!(ready["resourceState"]["enabled"], true);
        assert_eq!(ready["resourceState"]["setActiveTools"], "unsupported");
        assert!(ready["supportedCommands"]
            .as_array()
            .unwrap()
            .contains(&json!("get_resource_state")));
        host
    }
    async fn start_legacy(root: &Path) -> Self {
        let error = root.join("legacy.stderr");
        let mut child = command(root)
            .args(["--no-session", "--tools", "read,echo"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&error).unwrap())
            .spawn()
            .unwrap();
        let mut host = Self {
            error,
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
        };
        let ready = host.until(|v| v["type"] == "ready").await;
        assert_eq!(ready["resourceState"]["enabled"], false);
        host
    }
    async fn until(&mut self, test: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let line = self.output.next_line().await.unwrap().unwrap_or_else(|| {
                    panic!(
                        "native RPC closed: {}",
                        std::fs::read_to_string(&self.error).unwrap()
                    )
                });
                let value: Value = serde_json::from_str(&line).unwrap();
                if test(&value) {
                    return value;
                }
            }
        })
        .await
        .expect("native RPC timeout")
    }
    async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn request(&mut self, kind: &str, extra: Value) -> Value {
        let mut value = extra;
        value["type"] = json!(kind);
        value["id"] = json!(kind);
        self.send(value).await;
        self.until(|v| v["id"] == kind).await
    }
    async fn state(&mut self) -> Value {
        let response = self
            .request("get_resource_state", json!({"version":1}))
            .await;
        assert_eq!(response["success"], true, "{response}");
        response["data"].clone()
    }
    async fn prompt(&mut self, text: &str) {
        let accepted = self.request("prompt", json!({"message":text})).await;
        assert_eq!(accepted["success"], true, "{accepted}");
        self.until(|v| v["type"] == "agent_settled").await;
    }
    async fn stop(mut self) {
        self.send(json!({"type":"stop"})).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        // Also wait on assertion/panic paths so Windows releases loaded DLLs
        // before the test-owned TempDir attempts cleanup. Never touch other hosts.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.start_kill();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().ok().flatten().is_none()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

async fn body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(n) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break n + 4;
        }
    };
    let length: usize = String::from_utf8_lossy(&bytes[..end])
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}
fn config(main: &str, tools: Value) -> Value {
    json!({"version":1, "main_package":{"name":main,"instructions":["prompts/persistent.md"]},
        "skills":[format!("extra/extra-{main}/SKILL.md")], "instructions":[format!("{main}-instruction.md")], "tools":tools})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_native_hosts_exact_model_input_invocation_none_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let fixture_build = Fixture::build().await;
    let fixture = fixture_build.library.clone();
    let dll = fixture.file_name().unwrap().to_str().unwrap();
    for name in ["a", "b", "unselected"] {
        let package = root.join(format!(".rpi/packages/{name}"));
        skill(
            package.join(format!("skills/package-{name}/SKILL.md")),
            &format!("package-{name}"),
            &format!("PACKAGE_{name}_MARKER"),
        );
        put(
            package.join("prompts/persistent.md"),
            format!("PERSISTENT_{name}_MARKER"),
        );
        put(
            package.join("prompts/template.md"),
            "UNSELECTED_TEMPLATE_MARKER",
        );
        skill(
            root.join(format!("extra/extra-{name}/SKILL.md")),
            &format!("extra-{name}"),
            &format!("EXTRA_{name}_MARKER"),
        );
        put(
            root.join(format!("{name}-instruction.md")),
            format!("INSTRUCTION_{name}_MARKER"),
        );
    }
    std::fs::create_dir_all(root.join(".rpi/packages/a/extensions")).unwrap();
    std::fs::copy(&fixture, root.join(".rpi/packages/a/extensions").join(dll)).unwrap();
    put(root.join(".rpi/packages.json"), json!({"packages":[
        {"name":"a","version":"1","source":"fixture","cargo_package":"plugin-stub","artifacts":[dll]},
        {"name":"b","version":"1","source":"fixture","cargo_package":"none","artifacts":[]},
        {"name":"unselected","version":"1","source":"fixture","cargo_package":"none","artifacts":[]}
    ]}).to_string());
    for base in [root.join(".rpi"), root.join("agent")] {
        skill(
            base.join("skills/unselected/SKILL.md"),
            "unselected",
            "UNSELECTED_SKILL_MARKER",
        );
        put(base.join("prompts/leak.md"), "UNSELECTED_TEMPLATE_MARKER");
        put(base.join("SYSTEM.md"), "UNSELECTED_SYSTEM_MARKER");
        put(base.join("APPEND_SYSTEM.md"), "UNSELECTED_APPEND_MARKER");
        std::fs::create_dir_all(base.join("extensions")).unwrap();
        std::fs::copy(&fixture, base.join("extensions").join(dll)).unwrap();
    }
    skill(
        root.join("plugin-stub-discovered/SKILL.md"),
        "discovered-echo",
        "UNSELECTED_CALLBACK_MARKER",
    );
    put(root.join("AGENTS.md"), "UNSELECTED_CONTEXT_MARKER");
    // Exact default ignores saved tool defaults, without changing extension loading.
    put(
        root.join("agent/settings.json"),
        r#"{"defaultTools":["docs"],"retry":{"enabled":false}}"#,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    put(root.join("agent/models.json"), json!({"providers":{"fixture":{
        "api":"openai-completions", "baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "apiKey":"fixture-only", "models":[{"id":"test","contextWindow":131072,"maxTokens":256}]
    }}}).to_string());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        // a: tool + completion; b, none, empty-only, default, legacy: completion.
        for _ in 0..7 {
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
            let messages = request["messages"].to_string();
            let legacy = messages.contains("run-legacy");
            assert_eq!(system.contains("UNSELECTED"), legacy, "{system}");
            let is_a = messages.contains("run-a");
            let is_none = messages.contains("run-none") || messages.contains("run-empty");
            let is_default = messages.contains("run-default");
            if legacy {
                assert!(
                    system.contains("UNSELECTED_SYSTEM_MARKER")
                        && system.contains("UNSELECTED_SKILL_MARKER"),
                    "{system}"
                );
                assert!(system.contains("UNSELECTED_CALLBACK_MARKER"), "{system}");
            } else if is_a || is_default || messages.contains("run-none") {
                assert!(
                    system.contains("PERSISTENT_a_MARKER")
                        && system.contains("INSTRUCTION_a_MARKER"),
                    "{system}"
                );
                assert_eq!(
                    system.contains("PACKAGE_a_MARKER") && system.contains("EXTRA_a_MARKER"),
                    !is_none,
                    "{system}"
                );
                assert!(!system.contains("PACKAGE_b_MARKER"));
            } else {
                assert!(
                    system.contains("PERSISTENT_b_MARKER")
                        && !system.contains("PERSISTENT_a_MARKER"),
                    "{system}"
                );
                assert_eq!(system.contains("PACKAGE_b_MARKER"), !is_none, "{system}");
            }
            let tools: Vec<_> = request["tools"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .map(|v| v["function"]["name"].as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            if is_none {
                assert!(tools.is_empty(), "{request}");
            } else if is_default {
                assert!(
                    tools.contains(&"echo".into())
                        && tools.contains(&"bash".into())
                        && tools.contains(&"read".into())
                );
            } else if is_a || legacy {
                assert_eq!(tools, ["read", "echo"]);
            } else {
                assert_eq!(tools, ["read"]);
            }
            let tool_result = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool");
            let frame = if is_a && tool_result.is_none() {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"fixture-echo","type":"function","function":{"name":"echo","arguments":"{\"text\":\"ABI_MARKER\"}"}}]},"finish_reason":"tool_calls"}]})
            } else {
                if is_a {
                    assert!(
                        tool_result.unwrap()["content"]
                            .to_string()
                            .contains("echo: ABI_MARKER"),
                        "{request}"
                    );
                }
                json!({"choices":[{"index":0,"delta":{"content":"fixture complete"},"finish_reason":"stop"}]})
            };
            let response = format!("data: {frame}\n\ndata: [DONE]\n\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
            requests.push(request);
        }
        requests
    });
    put(
        root.join("a.json"),
        config("a", json!({"mode":"only","names":["read","echo"]})).to_string(),
    );
    put(
        root.join("b.json"),
        config("b", json!({"mode":"only","names":["read"]})).to_string(),
    );
    let mut a = Host::start(root, "a.json", "resource-a").await;
    let mut b = Host::start(root, "b.json", "resource-b").await;
    let sa = a.state().await;
    let sb = b.state().await;
    assert_ne!(sa["hostId"], sb["hostId"]);
    assert_eq!(sa["sessionId"], "resource-a");
    assert_eq!(sa["loadedExtensions"].as_array().unwrap().len(), 1);
    assert_eq!(sb["loadedExtensions"], json!([]));
    assert_eq!(sa["modelEffectiveTools"], json!(["read", "echo"]));
    assert_eq!(sa["loadedSkills"].as_array().unwrap().len(), 2);
    assert!(sa["loadedSkills"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["modelVisible"] == true));
    assert_eq!(sa["instructions"][0]["source"], "package:a");
    assert_eq!(sa["package"]["name"], "a");
    assert_eq!(sa["package"]["version"], "1");
    assert_eq!(sa["package"]["source"], "fixture");
    assert_eq!(sa["package"]["cargo_package"], "plugin-stub");
    assert_eq!(
        PathBuf::from(sa["package"]["root"].as_str().unwrap()),
        std::fs::canonicalize(root.join(".rpi/packages/a")).unwrap()
    );
    assert_eq!(
        sa["instructions"][0]["sha256"],
        json!(format!(
            "{:x}",
            sha2::Sha256::digest(b"PERSISTENT_a_MARKER")
        ))
    );
    assert_eq!(sa["promptTemplates"], json!([]));
    assert!(!sa.to_string().contains("PERSISTENT_a_MARKER"));
    assert!(!sa.to_string().contains("fixture-only"));
    assert_eq!(
        a.request("get_resource_state", json!({"version":2})).await["success"],
        false
    );
    assert_eq!(
        a.request("get_resource_state", json!({"unexpected":true}))
            .await["success"],
        false
    );
    a.send(json!({"type":"get_resource_state","id":"x".repeat(5000)}))
        .await;
    let bounded_error = a.until(|v| v["command"] == "get_resource_state").await;
    assert_eq!(bounded_error["success"], false);
    assert!(bounded_error.get("id").is_none_or(Value::is_null));
    assert!(bounded_error.to_string().len() < 4096);
    assert_eq!(
        a.request("set_active_tools", json!({"tools":[]})).await["success"],
        false
    );
    assert_eq!(
        a.request("configure_external_tools", json!({"tools":[]}))
            .await["success"],
        false
    );
    tokio::join!(a.prompt("run-a"), b.prompt("run-b"));
    a.stop().await;
    b.stop().await;
    let mut reopened = Host::start(root, "a.json", "resource-a").await;
    let restored = reopened.state().await;
    assert_eq!(restored["loadedSkills"], sa["loadedSkills"]);
    assert_eq!(restored["loadedExtensions"], sa["loadedExtensions"]);
    assert_ne!(restored["hostId"], sa["hostId"]);
    let history = reopened.request("get_messages", json!({})).await;
    assert!(
        history.to_string().contains("run-a") && history.to_string().contains("echo: ABI_MARKER"),
        "{history}"
    );
    reopened.stop().await;
    for (name, mode) in [
        ("none", json!({"mode":"none"})),
        ("empty", json!({"mode":"only","names":[]})),
        ("default", json!({"mode":"default"})),
    ] {
        let main = if name == "empty" { "b" } else { "a" };
        put(
            root.join(format!("{name}.json")),
            config(main, mode).to_string(),
        );
        let mut host =
            Host::start(root, &format!("{name}.json"), &format!("resource-{name}")).await;
        let state = host.state().await;
        if name == "none" {
            assert_eq!(state["loadedExtensions"].as_array().unwrap().len(), 1);
            assert!(state["registeredTools"]
                .as_array()
                .unwrap()
                .contains(&json!("echo")));
        }
        if name != "default" {
            assert_eq!(state["modelEffectiveTools"], json!([]));
            assert!(state["registeredTools"].as_array().unwrap().len() >= 5);
            assert!(state["loadedSkills"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["modelVisible"] == false));
            assert_eq!(
                host.request("set_active_tools", json!({"tools":["read"]}))
                    .await["success"],
                false
            );
        }
        host.prompt(&format!("run-{name}")).await;
        host.stop().await;
    }
    let mut additional = config("b", json!({"mode":"only", "names":["echo"]}));
    additional["extensions"] = json!([fixture]);
    put(root.join("additional.json"), additional.to_string());
    let mut extra = Host::start(root, "additional.json", "resource-additional").await;
    let state = extra.state().await;
    assert_eq!(state["loadedExtensions"].as_array().unwrap().len(), 1);
    assert_eq!(state["modelEffectiveTools"], json!(["echo"]));
    assert!(state["loadedSkills"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["modelVisible"] == false));
    extra.stop().await;
    let mut legacy = Host::start_legacy(root).await;
    assert_eq!(legacy.request("append_entry",json!({"customType":"rpi.value","data":{"key":"rpi.resource-selection.v1","value":{"forged":true}}})).await["success"],false);
    assert_eq!(
        legacy.request("get_resource_state", json!({})).await["success"],
        false
    );
    assert_eq!(
        legacy
            .request("set_active_tools", json!({"tools":[]}))
            .await["success"],
        true
    );
    let actual = legacy.request("get_state", json!({})).await;
    assert_eq!(actual["data"]["activeTools"], json!([]));
    assert_eq!(
        legacy.request("get_available_tools", json!({})).await["data"]["activeTools"],
        json!([])
    );
    legacy.prompt("run-legacy").await;
    legacy.stop().await;
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 7);
    // SAME native session cannot silently change modes, or return to discovery.
    let output = command(root)
        .args(["--resource-config", "b.json", "--session-id", "resource-a"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("selection differs"));
    let output = command(root)
        .args(["--session-id", "resource-a"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    skill(
        root.join("extra/extra-a/SKILL.md"),
        "extra-a",
        "CHANGED_MARKER",
    );
    let output = command(root)
        .args(["--resource-config", "a.json", "--session-id", "resource-a"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("inventory changed"));
    fixture_build.assert_registrations(&root.join("registrations.log"));
}

#[tokio::test]
async fn malformed_missing_unknown_and_duplicate_selection_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    put(root.join("agent/models.json"), json!({"providers":{"fixture":{"api":"openai-completions","apiKey":"fixture-only","baseUrl":"http://127.0.0.1:1","models":[{"id":"test"}]}}}).to_string());
    let base = json!({"version":1,"tools":{"mode":"none"}});
    let mut cases = vec![
        json!({}),
        json!({"version":2,"tools":{"mode":"none"}}),
        json!({"version":1,"tools":{}}),
    ];
    for (field, value) in [
        ("unexpected", json!(true)),
        ("skills", json!(["missing/SKILL.md"])),
        ("extensions", json!(["missing.dll"])),
        ("instructions", json!(["missing.md"])),
        ("main_package", json!({"name":"missing"})),
        ("tools", json!({"mode":"only","names":["unknown"]})),
        ("tools", json!({"mode":"only","names":["read","read"]})),
        ("tools", json!({"mode":"none","names":[]})),
    ] {
        let mut case = base.clone();
        case[field] = value;
        cases.push(case);
    }
    put(root.join("instruction.md"), "instruction");
    let mut duplicate = base.clone();
    duplicate["instructions"] = json!(["instruction.md", "./instruction.md"]);
    cases.push(duplicate);
    put(
        root.join("malformed/SKILL.md"),
        "---\nname: [unterminated\n---\n",
    );
    skill(root.join("one/collision/SKILL.md"), "collision", "first");
    skill(root.join("two/collision/SKILL.md"), "collision", "second");
    std::fs::create_dir_all(root.join("empty-skills")).unwrap();
    let invalid_library = format!("invalid.{}", std::env::consts::DLL_EXTENSION);
    put(root.join(&invalid_library), "not a library");
    for paths in [
        json!(["malformed/SKILL.md"]),
        json!(["empty-skills"]),
        json!(["one", "two"]),
        json!(["one", "./one"]),
    ] {
        let mut case = base.clone();
        case["skills"] = paths;
        cases.push(case);
    }
    let mut case = base.clone();
    case["extensions"] = json!([invalid_library]);
    cases.push(case);
    let mut case = base.clone();
    case["instructions"] = json!(["oversized.md"]);
    cases.push(case);
    put(root.join("oversized.md"), "x".repeat(65537));
    for case in cases {
        put(root.join("bad.json"), case.to_string());
        let output = command(root)
            .args(["--resource-config", "bad.json", "--no-session"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "accepted {case}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("\"type\":\"ready\""));
    }
    for content in ["{", &" ".repeat(65537)] {
        put(root.join("bad.json"), content);
        assert!(!command(root)
            .args(["--resource-config", "bad.json", "--no-session"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap()
            .status
            .success());
    }
    put(root.join("valid.json"), base.to_string());
    let valid = command(root)
        .args(["--resource-config", "valid.json", "--no-session"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(String::from_utf8_lossy(&valid.stdout).contains("\"enabled\":true"));
    for flag in [
        "--no-skills",
        "--no-extensions",
        "--no-context-files",
        "--no-tools",
        "--no-approve",
    ] {
        assert!(
            !command(root)
                .args(["--resource-config", "valid.json", flag])
                .stdin(Stdio::null())
                .output()
                .await
                .unwrap()
                .status
                .success(),
            "accepted conflict {flag}"
        );
    }
}

#[test]
fn exact_mode_reload_is_restart_required_and_preparation_is_frozen() {
    const CHILD: &str = "RPI_RESOURCE_RELOAD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "exact_mode_reload_is_restart_required_and_preparation_is_frozen",
                "--nocapture",
            ])
            .env(CHILD, root.path())
            .env("RPI_CODING_AGENT_DIR", root.path().join("agent"))
            .env_remove("RPI_EXTENSIONS_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    put(
        root.join(".rpi/packages/metadata/persistent.md"),
        "frozen instructions",
    );
    put(root.join(".rpi/packages.json"),json!({"packages":[{"name":"metadata","version":"captured-version","source":"captured-source","cargo_package":"captured-identity","artifacts":[]}]}).to_string());
    put(
        root.join("resources.json"),
        r#"{"version":1,"main_package":{"name":"metadata","instructions":["persistent.md"]},"tools":{"mode":"none"}}"#,
    );
    skill(
        root.join(".rpi/skills/unselected/SKILL.md"),
        "unselected",
        "UNSELECTED_MARKER",
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use rpi_cli::{
            args::{Args, Mode},
            session::{build_prepared, prepare_extensions, reload_extension_resources},
        };
        let args = Args {
            resource_config: Some(root.join("resources.json")),
            mode: Mode::Rpc,
            no_session: true,
            ..Default::default()
        };
        let uncreated_root=root.join("preflight-only-session-root");
        let persistent_args=Args { no_session:false, session_id:Some("preflight-only".into()),session_dir:Some(uncreated_root.clone()), ..args.clone() };
        drop(prepare_extensions(&persistent_args,&root,true).unwrap());
        assert!(!uncreated_root.exists(),"preflight must not create a session root/file/lock");
        let prepared = prepare_extensions(&args, &root, true).unwrap();
        put(root.join(".rpi/packages.json"),json!({"packages":[{"name":"metadata","version":"changed-version","source":"changed-source","cargo_package":"changed-identity","artifacts":[]}]}).to_string());
        // The config is a frozen startup snapshot, not a second discovery opportunity.
        put(
            root.join("resources.json"),
            r#"{"version":1,"tools":{"mode":"default"}}"#,
        );
        let resolved = rpi_cli::provider::resolve(
            Some("anthropic"),
            Some(rpi_cli::provider::DEFAULT_MODEL_ID),
            None,
            Some("fixture-only"),
            None,
        )
        .unwrap();
        let (harness, _, mut context) = build_prepared(&resolved, &args, &root, true, prepared)
            .await
            .unwrap();
        let before = rpi_cli::resource_selection::get_resource_state(&harness)
            .await
            .unwrap();
        assert_eq!(before["modelEffectiveTools"], json!([]));
        assert_eq!(before["loadedSkills"], json!([]));
        assert_eq!(before["package"]["version"],"captured-version");
        assert_eq!(before["package"]["source"],"captured-source");
        assert_eq!(before["package"]["cargo_package"],"captured-identity");
        assert_eq!(before["instructions"][0]["sha256"],json!(format!("{:x}",sha2::Sha256::digest(b"frozen instructions"))));
        let host = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_host();
        for key in [
            "rpi.resource-selection.v1",
            "rpi.resource-inventory.v1",
            "rpi.resource-selection.v2",
        ] {
            for value in [Value::Null, json!({"forged":true})] {
                assert!(host
                    .append_entry(
                        json!({"customType":"rpi.value","data":{"key":key,"value":value}})
                    )
                    .await
                    .unwrap_err()
                    .contains("host-owned"));
            }
        }
        host.append_entry(
            json!({"customType":"rpi.value","data":{"key":"user.preference","value":"kept"}}),
        )
        .await
        .unwrap();
        host.append_entry(json!({"customType":"unrelated","data":{"n":1}}))
            .await
            .unwrap();
        let values = rpi_harness::session::SessionValues::load(harness.session())
            .await
            .unwrap();
        assert_eq!(values.get("user.preference"), Some(&json!("kept")));
        assert!(values
            .get("rpi.resource-selection.v1")
            .unwrap()
            .get("inventory")
            .is_some());
        let outcome = reload_extension_resources(&harness, &context).await;
        assert!(outcome.had_errors && outcome.summary.contains("restart-required"));
        context.args.resource_config = None;
        let outcome = reload_extension_resources(&harness, &context).await;
        assert!(outcome.had_errors);
        assert_eq!(
            before,
            rpi_cli::resource_selection::get_resource_state(&harness)
                .await
                .unwrap()
        );
    });
}
