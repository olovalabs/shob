use super::resource_lifecycle::{fixture_model_config, session_files};
use super::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_reader_missing_selected_resources_preserves_real_history_without_registration_or_startup(
) {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture_model_config(root);
    let selected = root.join(format!("selected.{}", std::env::consts::DLL_EXTENSION));
    std::fs::copy(&fixture.library, &selected).unwrap();
    put(root.join("chosen.md"), "selected native reader input");
    put(root.join("exact.json"),json!({"version":1,"instructions":["chosen.md"],"extensions":[selected],"tools":{"mode":"none"}}).to_string());
    let mut host = Host::start(root, "exact.json", "reader-real").await;
    host.prompt("saved reader retained real native user message")
        .await;
    let snapshot = host.state().await;
    let files = session_files(root);
    let path = files
        .keys()
        .find(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && !p.to_string_lossy().ends_with(".progress.jsonl")
        })
        .unwrap();
    let query = json!({"version":1,"sessionId":snapshot["sessionId"],"sessionRoot":root.join("sessions"),"sessionPath":path,"limit":128,"customTypes":[],"cursor":null});
    put(root.join("reader.json"), query.to_string());
    async fn read(root: &Path) -> Value {
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
                .current_dir(root)
                .env("RPI_CODING_AGENT_DIR", root.join("agent"))
                .env(
                    "RPI_PLUGIN_STUB_REGISTRATION_LOG",
                    root.join("registrations.log"),
                )
                .args(["--resource-session-read", "reader.json"])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)))
    }
    assert_eq!(read(root).await["error"]["code"], "busy");
    host.stop().await;
    std::fs::remove_file(&selected).unwrap();
    std::fs::remove_file(root.join("chosen.md")).unwrap();
    let unselected = root
        .join("agent/extensions")
        .join(fixture.library.file_name().unwrap());
    std::fs::create_dir_all(unselected.parent().unwrap()).unwrap();
    std::fs::copy(&fixture.library, &unselected).unwrap();
    for file in ["settings.json", "models.json", "auth.json", "trust.json"] {
        put(
            root.join("agent").join(file),
            "invalid JSON: ordinary startup must reject",
        );
    }
    let before = session_files(root);
    let registrations = std::fs::read(root.join("registrations.log")).unwrap();
    let view = read(root).await;
    assert_eq!(view["success"], true, "{view}");
    assert!(view["data"]["messages"]
        .to_string()
        .contains("saved reader retained real native user message"));
    assert_eq!(view["data"]["binding"], snapshot["binding"]);
    assert_eq!(session_files(root), before);
    assert_eq!(
        std::fs::read(root.join("registrations.log")).unwrap(),
        registrations
    );
}
