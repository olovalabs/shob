//! Stage B management must not widen the experimental print-only CLI contract.
use super::*;
use rpi_harness::session::{experimental_native::ExperimentalNativeRepo, types::*};

#[tokio::test]
async fn resource_inspection_is_nonexecuting_and_preserves_native_transcript_ownership() {
    let f = Fixture::new();
    let before = f.sentinels();
    let source = f.temp.path().join("source");
    std::fs::create_dir_all(source.join("skills/demo")).unwrap();
    std::fs::create_dir_all(source.join("prompts")).unwrap();
    std::fs::write(
        source.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: isolated native selection\n---\nNATIVE_SKILL\n",
    )
    .unwrap();
    std::fs::write(
        source.join("prompts/instructions.md"),
        "NATIVE_INSTRUCTIONS",
    )
    .unwrap();
    // Management copies a bounded non-DLL bundle; Cargo metadata grants no execution.
    std::fs::write(
        source.join("Cargo.toml"),
        "[package]\nname='must-not-build'\nversion='0.0.1'\nbuild='build.rs'\n",
    )
    .unwrap();
    std::fs::write(
        source.join("build.rs"),
        "compile_error!(\"inspection must not compile\");",
    )
    .unwrap();
    let installed = output(
        f.command()
            .args([
                "resource", "install", "package", "demo", "--scope", "project", "--path",
            ])
            .arg(&source),
    )
    .await;
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let config = f.temp.path().join("selection.json");
    std::fs::write(&config, json!({"version":1,"main_package":{"name":"demo","instructions":["prompts/instructions.md"]},"tools":{"mode":"none"}}).to_string()).unwrap();
    // Exact selection's existing documented CLI scope is trusted RPC only;
    // the experimental backend remains fresh text/json print only. Do not
    // extend either scope just because management installed selectable paths.
    let refused = output(
        f.command()
            .args([
                "--offline",
                "--provider=anthropic",
                "--api-key=fixture-only",
                "--print",
                "--experimental-native-store=native",
                "--resource-config",
            ])
            .arg(&config),
    )
    .await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("requires trusted --mode rpc"));
    assert!(!f.native.exists());
    assert_eq!(f.sentinels(), before);

    let result = output(f.command().args([
        "--offline",
        "--no-extensions",
        "--no-skills",
        "--no-context-files",
        "--no-prompt-templates",
        "--no-builtin-tools",
        "--provider=anthropic",
        "--api-key=fixture-only",
        "--print",
        "--name=native ownership sentinel",
        "--experimental-native-store=native",
    ]))
    .await;
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(f.sentinels(), before);
    let repo = ExperimentalNativeRepo::new(f.native.clone()).await.unwrap();
    let sessions = repo.list().await.unwrap();
    assert_eq!(sessions.len(), 1);
    let storage = repo.open(&sessions[0]).await.unwrap();
    assert_eq!(
        storage.get_name().await.unwrap().as_deref(),
        Some("native ownership sentinel")
    );
    drop(storage);
    let artifact = std::fs::read_dir(&f.native)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(&artifact).unwrap();
    let inspected = output(f.command().args([
        "resource", "inspect", "package", "demo", "--scope", "project",
    ]))
    .await;
    assert!(inspected.status.success());
    let inspection: Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspection["success"], true);
    assert_eq!(inspection["data"]["resources"][0]["loaded"], Value::Null);
    assert_eq!(std::fs::read(&artifact).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&f.native).unwrap().count(), 1);
    assert_eq!(repo.list().await.unwrap().len(), 1);
    assert_eq!(
        repo.open(&sessions[0])
            .await
            .unwrap()
            .get_name()
            .await
            .unwrap()
            .as_deref(),
        Some("native ownership sentinel")
    );
    assert_eq!(f.sentinels(), before);
}
