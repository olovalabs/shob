//! Build rpi-providers in the independent rpi-package workspace, then set
//! RPI_TEST_PROVIDER_LIBRARY to its DLL/so/dylib and RPI_TEST_PROVIDER_PACKAGE
//! to its package directory. The host has no vendor crate dependency.
//! Then run cargo test -p rpi-cli --test provider_packages.
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn library(name: &str) -> Option<PathBuf> {
    if name == "rpi_providers" {
        let path = PathBuf::from(std::env::var_os("RPI_TEST_PROVIDER_LIBRARY")?);
        assert!(
            path.is_file(),
            "RPI_TEST_PROVIDER_LIBRARY must name the built bundle"
        );
        return Some(path);
    }
    let profile = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let file = if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    [profile.join(&file), profile.join("deps").join(file)]
        .into_iter()
        .find(|path| path.is_file())
}

fn command_base(temp: &tempfile::TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rpi"));
    cmd.current_dir(temp.path())
        .stdin(Stdio::null())
        .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
        .env("RPI_OFFLINE", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("LLAMA_CPP_API_KEY")
        .env_remove("PROXY_API_KEY")
        .env_remove("DEEPSEEK_API_KEY");
    cmd
}

fn command(temp: &tempfile::TempDir, library: &std::path::Path) -> Command {
    let mut cmd = command_base(temp);
    cmd.arg("--extension").arg(library);
    cmd
}

fn auth_command(temp: &tempfile::TempDir, library: &std::path::Path, provider: &str) -> Command {
    let mut cmd = command_base(temp);
    cmd.args([
        "auth",
        "check",
        "--provider",
        provider,
        "--json",
        "--extension",
    ])
    .arg(library);
    cmd
}

fn assert_auth_output(output: std::process::Output, ready: bool, source: &str) {
    assert_eq!(
        output.status.code(),
        Some(if ready { 0 } else { 1 }),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ready"], ready);
    assert_eq!(value["source"], source);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret-value"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-value"));
}

#[test]
fn auth_check_uses_package_policy_precedence_and_provider_isolation() {
    let Some(library) = library("rpi_providers") else {
        eprintln!("SKIP: build rpi-providers and set RPI_TEST_PROVIDER_LIBRARY first");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .output()
            .unwrap(),
        false,
        "none",
    );
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENAI_API_KEY", "secret-value-openai")
            .env("ANTHROPIC_API_KEY", "secret-value-anthropic")
            .output()
            .unwrap(),
        false,
        "none",
    );
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENROUTER_API_KEY", "secret-value-env")
            .output()
            .unwrap(),
        true,
        "OPENROUTER_API_KEY",
    );
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENROUTER_API_KEY", "secret-value-env")
            .arg("--no-extensions")
            .output()
            .unwrap(),
        false,
        "none",
    );

    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        r#"{"providers":{"openrouter":{"apiKey":"secret-value-config","models":[]}}}"#,
    )
    .unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENROUTER_API_KEY", "secret-value-env")
            .output()
            .unwrap(),
        true,
        "models.json",
    );
    std::fs::write(
        agent.join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"secret-value-stored"}}"#,
    )
    .unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENROUTER_API_KEY", "secret-value-env")
            .output()
            .unwrap(),
        true,
        "auth.json",
    );
    std::fs::write(
        agent.join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"$RPI_TEST_MISSING_PACKAGE_AUTH"}}"#,
    )
    .unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .output()
            .unwrap(),
        true,
        "models.json",
    );
    std::fs::write(
        agent.join("auth.json"),
        r#"{"openrouter":{"type":"oauth","access":"secret-value-access","expires":1}}"#,
    )
    .unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .output()
            .unwrap(),
        true,
        "models.json",
    );
    std::fs::write(
        agent.join("auth.json"),
        r#"{"openrouter":{"type":"oauth","access":"secret-value-access","expires":0}}"#,
    )
    .unwrap();
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .output()
            .unwrap(),
        true,
        "oauth",
    );
}

#[test]
fn auth_check_honors_anonymous_packages_and_logout_keeps_environment_keys() {
    for (library_name, provider) in [("rpi_providers", "proxy"), ("rpi_providers", "llama-cpp")] {
        let Some(library) = library(library_name) else {
            continue;
        };
        let temp = tempfile::tempdir().unwrap();
        assert_auth_output(
            auth_command(&temp, &library, provider).output().unwrap(),
            true,
            "anonymous",
        );
        assert_auth_output(
            auth_command(&temp, &library, provider)
                .arg("--no-extensions")
                .output()
                .unwrap(),
            false,
            "none",
        );
    }
    let Some(library) = library("rpi_providers") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"secret-value-stored"}}"#,
    )
    .unwrap();
    let output = command_base(&temp)
        .args(["auth", "logout", "--provider", "openrouter", "--extension"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .output()
            .unwrap(),
        false,
        "none",
    );
    assert_auth_output(
        auth_command(&temp, &library, "openrouter")
            .env("OPENROUTER_API_KEY", "secret-value-env")
            .output()
            .unwrap(),
        true,
        "OPENROUTER_API_KEY",
    );
}

#[test]
fn package_catalog_is_available_without_credentials_and_respects_no_extensions() {
    let Some(library) = library("rpi_providers") else {
        eprintln!("SKIP: build rpi-providers and set RPI_TEST_PROVIDER_LIBRARY first");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp, &library)
        .args(["--list-models", "openrouter"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("openrouter/auto"));
    let output = command(&temp, &library)
        .args(["--no-extensions", "--list-models", "openrouter"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("openrouter/auto"));
}

#[test]
fn openrouter_package_does_not_inherit_openai_credentials() {
    let Some(library) = library("rpi_providers") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp, &library)
        .env("OPENAI_API_KEY", "unrelated-openai-key")
        .args([
            "--provider",
            "openrouter",
            "--model",
            "openrouter/auto",
            "--print",
            "--no-session",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No API key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("unrelated-openai-key"));
}

#[tokio::test]
async fn packaged_providers_stream_through_the_real_dll_and_cli() {
    for (name, provider, model, source) in [
        (
            "rpi_providers",
            "openrouter",
            "openrouter/auto",
            "environment",
        ),
        (
            "rpi_providers",
            "openrouter",
            "openrouter/auto",
            "models.json",
        ),
        (
            "rpi_providers",
            "openrouter",
            "openrouter/auto",
            "auth.json",
        ),
        ("rpi_providers", "openrouter", "openrouter/auto", "oauth"),
        (
            "rpi_providers",
            "openrouter",
            "openrouter/auto",
            "expired-fallback",
        ),
        ("rpi_providers", "openrouter", "openrouter/auto", "cli"),
        ("rpi_providers", "llama-cpp", "local", "anonymous"),
        ("rpi_providers", "proxy", "default", "anonymous"),
    ] {
        let auth = source != "anonymous";
        let Some(library) = library(name) else {
            eprintln!("SKIP: build {name} first");
            continue;
        };
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0u8; 8192];
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0, "request closed before body");
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_string)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("POST /v1/chat/completions "));
            if auth {
                assert!(text
                    .to_ascii_lowercase()
                    .contains("authorization: bearer package-key"));
                assert!(
                    text.to_ascii_lowercase().contains("x-title: rpi"),
                    "the package factory must handle this request"
                );
            } else {
                assert!(!text.to_ascii_lowercase().contains("authorization:"));
            }
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let request_body: serde_json::Value =
                serde_json::from_slice(&request[body_start..]).unwrap();
            assert_eq!(
                request_body["model"], model,
                "preserve the provider's wire model id"
            );
            assert!(!text.contains("unrelated-openai-key"));
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"package \"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"works\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut cmd = command(&temp, &library);
        cmd.env("OPENAI_API_KEY", "unrelated-openai-key").args([
            "--provider",
            provider,
            "--model",
            model,
            "--base-url",
            &base,
            "--print",
            "--no-session",
            "--no-tools",
            "hello",
        ]);
        if auth {
            cmd.env(
                "OPENROUTER_API_KEY",
                if matches!(source, "environment" | "expired-fallback") {
                    "package-key"
                } else {
                    "lower-priority-env"
                },
            );
            let agent = temp.path().join("agent");
            std::fs::create_dir_all(&agent).unwrap();
            match source {
                "models.json" | "auth.json" | "oauth" | "cli" => {
                    let key = if source == "models.json" {
                        "package-key"
                    } else {
                        "lower-priority-config"
                    };
                    std::fs::write(
                        agent.join("models.json"),
                        serde_json::json!({"providers":{"openrouter":{"apiKey":key,"models":[]}}})
                            .to_string(),
                    )
                    .unwrap();
                }
                _ => {}
            }
            match source {
                "auth.json" => std::fs::write(
                    agent.join("auth.json"),
                    r#"{"openrouter":{"type":"api_key","key":"package-key"}}"#,
                )
                .unwrap(),
                "oauth" => std::fs::write(
                    agent.join("auth.json"),
                    r#"{"openrouter":{"type":"oauth","access":"package-key","expires":0}}"#,
                )
                .unwrap(),
                "expired-fallback" => std::fs::write(
                    agent.join("auth.json"),
                    r#"{"openrouter":{"type":"oauth","access":"expired-key","expires":1}}"#,
                )
                .unwrap(),
                "cli" => {
                    cmd.args(["--api-key", "package-key"]);
                }
                _ => {}
            }
        }
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || cmd.output().unwrap()),
        )
        .await
        .expect("CLI finishes")
        .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("package works"),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn reload_updates_the_catalog_endpoint_and_runtime_without_a_restart() {
    let Some(library) = library("rpi_providers") else {
        return;
    };
    // The child gets an isolated environment; parallel tests never mutate the
    // parent's config directory or credentials.
    if std::env::var_os("RPI_TEST_PACKAGE_RELOAD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reload_updates_the_catalog_endpoint_and_runtime_without_a_restart",
                "--nocapture",
            ])
            .env("RPI_TEST_PACKAGE_RELOAD", "1")
            .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
            .env("RPI_TEST_PACKAGE_CWD", temp.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("PROXY_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("models.json:"),
            "reload diagnostics must be returned to the UI: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let cwd = PathBuf::from(std::env::var_os("RPI_TEST_PACKAGE_CWD").unwrap());
    let extensions = vec![library];
    let args = rpi_cli::args::Args {
        extension: extensions,
        provider: Some("proxy".into()),
        model: Some("default".into()),
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let prepared = rpi_cli::session::prepare_extensions(&args, &cwd, true).unwrap();
    let resolved =
        rpi_cli::provider::resolve_with_extensions(&args, &cwd, true, &prepared.extension_session)
            .unwrap();
    let (harness, _events, mut context) =
        rpi_cli::session::build_prepared(&resolved, &args, &cwd, true, prepared)
            .await
            .unwrap();
    let before = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    let agent = rpi_cli::config::agent_dir().unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for expected in ["reload-key-0", "reload-key-1"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() < end + 4 + length {
                        continue;
                    }
                    assert!(headers.contains(&format!("authorization: bearer {expected}")));
                    break;
                }
            }
            let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"reload works\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    for index in 0..2 {
        std::fs::write(agent.join("models.json"), serde_json::json!({"providers":{"proxy":{
            "baseUrl":base,"apiKey":format!("reload-key-{index}"),"models":[{"id":"default","compat":{"misspelledReloadFlag":true}},{"id":"second"}]
        }}}).to_string()).unwrap();
        let outcome = rpi_cli::session::reload_extension_resources(&harness, &context).await;
        assert!(
            !outcome.had_errors,
            "{} {:?}",
            outcome.summary, outcome.details
        );
        assert!(outcome.had_warnings);
        assert_eq!(
            outcome
                .details
                .iter()
                .filter(|detail| detail.contains("misspelledReloadFlag"))
                .count(),
            1
        );
        assert!(context
            .catalog
            .read()
            .unwrap()
            .iter()
            .any(|model| model.id == "second"));
        // `/provider` reads the current session declarations and combines them
        // with models.json without requiring a provider runtime or credentials.
        let manifests =
            rpi_cli::provider::manifests_from_session(&context.extension_session.lock().unwrap());
        let discovery = rpi_cli::provider::catalog_with_manifests(&manifests).unwrap();
        assert!(manifests.iter().any(|manifest| manifest.id == "proxy"));
        assert!(discovery.iter().any(|model| model.provider == "proxy"
            && model.id == "second"
            && model.base_url == base));
        let model = harness.lane("main").get_model().await.unwrap();
        assert_eq!(model.base_url, base);
        assert!(
            model.headers.is_none(),
            "lane descriptors do not retain request auth"
        );
        assert!(context
            .catalog
            .read()
            .unwrap()
            .iter()
            .all(|model| model.headers.is_none()));
        let provider = context
            .providers
            .read()
            .unwrap()
            .iter()
            .find(|provider| provider.id() == "proxy")
            .cloned()
            .unwrap();
        let request_options = rpi_ai::SimpleStreamOptions {
            timeout: Some(Duration::from_secs(3)),
            max_retries: Some(0),
            ..Default::default()
        };
        let message = provider
            .stream_simple(&model, &rpi_ai::Context::new(vec![]), &request_options)
            .await
            .result()
            .await
            .unwrap();
        assert_eq!(message.stop_reason, rpi_ai::StopReason::Stop);
        assert_eq!(
            rpi_ai::types::Content::text_only(&message.content, ""),
            "reload works"
        );
        let options = rpi_ai::SimpleStreamOptions::default();
        options.signal.cancel();
        let result = provider
            .stream_simple(&model, &rpi_ai::Context::new(vec![]), &options)
            .await
            .result()
            .await
            .unwrap();
        assert_eq!(
            result.stop_reason,
            rpi_ai::StopReason::Aborted,
            "fresh runtime must not retain a stale snapshot"
        );
    }
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(!before.is_active());
    let local = context
        .catalog
        .read()
        .unwrap()
        .iter()
        .find(|model| model.provider == "llama-cpp")
        .cloned();
    if let Some(local) = local {
        // Startup-only overrides belong to the startup provider even when
        // `/reload` resolves a different provider after a live model switch.
        context.args.base_url = Some("http://localhost:9999/v1".into());
        context.args.api_key = Some("startup-proxy-key".into());
        harness.lane("main").set_model(local.clone()).await.unwrap();
        let outcome = rpi_cli::session::reload_extension_resources(&harness, &context).await;
        assert!(
            !outcome.had_errors,
            "{} {:?}",
            outcome.summary, outcome.details
        );
        let current = harness.lane("main").get_model().await.unwrap();
        assert_eq!(current.base_url, local.base_url);
        assert!(!current.headers.as_ref().is_some_and(|headers| headers
            .values()
            .any(|value| value.contains("startup-proxy-key"))));
        context.args.base_url = None;
        context.args.api_key = None;
        let proxy = context
            .catalog
            .read()
            .unwrap()
            .iter()
            .find(|model| model.provider == "proxy" && model.id == "default")
            .cloned()
            .unwrap();
        harness.lane("main").set_model(proxy).await.unwrap();
    }
    let before_failed = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    context.args.no_extensions = true;
    let outcome = rpi_cli::session::reload_extension_resources(&harness, &context).await;
    assert!(
        outcome.had_errors,
        "removing the active provider should fail cleanly"
    );
    let after_failed = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&before_failed, &after_failed));
    assert!(after_failed.is_active());
}

#[test]
fn linked_package_is_discovered_and_unlinked_cleanly() {
    if library("rpi_providers").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source =
        PathBuf::from(std::env::var_os("RPI_TEST_PROVIDER_PACKAGE").expect(
            "set RPI_TEST_PROVIDER_PACKAGE to packages/rpi-providers for link/unlink testing",
        ));
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(temp.path())
            .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
            .env("RPI_OFFLINE", "1")
            // package link spawns Cargo. Do not contend with the parent test
            // runner's target lock or another workspace's build artifacts.
            .env("CARGO_TARGET_DIR", temp.path().join("cargo-target"))
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let linked = run(&[
        "package",
        "link",
        "providers",
        "--path",
        source.to_str().unwrap(),
        "--debug",
        "--locked",
    ]);
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let catalog = run(&["--list-models"]);
    assert!(catalog.status.success());
    for model in ["deepseek-flash", "openrouter/auto", "local", "default"] {
        assert!(String::from_utf8_lossy(&catalog.stdout).contains(model));
    }
    let unlinked = run(&["package", "unlink", "providers"]);
    assert!(
        unlinked.status.success(),
        "{}",
        String::from_utf8_lossy(&unlinked.stderr)
    );
    let catalog = run(&["--list-models"]);
    for model in ["deepseek-flash", "openrouter/auto"] {
        assert!(!String::from_utf8_lossy(&catalog.stdout).contains(model));
    }
}
