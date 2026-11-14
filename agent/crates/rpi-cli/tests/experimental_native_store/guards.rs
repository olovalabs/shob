use super::*;
#[tokio::test]
async fn both_legacy_writers_reject_native_artifact_without_byte_or_sidecar_changes() {
    let f = Fixture::new();
    let artifact = f.temp.path().join("foreign.rpinative");
    // Reserved IDs are checked even without native feature. Fixture schema is
    // intentionally small; the writer must refuse before adopting any of it.
    for app in [0x5250494e, 0x5250494f, 0x52504951] {
        if artifact.exists() {
            std::fs::remove_file(&artifact).unwrap();
        }
        let db = rusqlite::Connection::open(&artifact).unwrap();
        db.execute_batch(&format!("PRAGMA application_id={app};CREATE TABLE root(marker TEXT);INSERT INTO root VALUES('sentinel');")).unwrap();
        drop(db);
        let before = std::fs::read(&artifact).unwrap();
        assert!(rpi_harness::session::SqliteSessionRepo::open(&artifact).is_err());
        assert_eq!(std::fs::read(&artifact).unwrap(), before);
        assert!(!PathBuf::from(format!("{}-wal", artifact.display())).exists());
        let fs = std::sync::Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            f.temp.path().to_owned(),
        ));
        let load = rpi_harness::session::jsonl::JsonlSessionStorage::load(
            fs,
            &artifact.to_string_lossy(),
            std::sync::Arc::new(rpi_harness::session::SystemClock),
            std::sync::Arc::new(rpi_harness::session::DefaultIdGenerator::new()),
        )
        .await;
        assert!(load.is_err());
        assert_eq!(std::fs::read(&artifact).unwrap(), before);
        assert!(!PathBuf::from(format!("{}.tmp", artifact.display())).exists());
        assert!(!PathBuf::from(format!("{}.progress.jsonl", artifact.display())).exists());
    }
}
#[cfg(feature = "experimental-native-store")]
#[tokio::test]
async fn native_creation_failure_never_falls_back_to_legacy_or_ephemeral() {
    let f = Fixture::new();
    let before = f.sentinels();
    std::fs::write(&f.native, "file sentinel").unwrap();
    let result = output(
        f.command()
            .args(["--print", "--experimental-native-store=native"]),
    )
    .await;
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(std::fs::read(&f.native).unwrap(), b"file sentinel");
    assert_eq!(f.sentinels(), before);
    std::fs::write(f.temp.path().join("blocked"), "parent sentinel").unwrap();
    let result = output(f.command().args([
        "--offline",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-builtin-tools",
        "--provider=anthropic",
        "--api-key=fixture-only",
        "--print",
        "--experimental-native-store=blocked/native",
    ]))
    .await;
    assert!(!result.status.success());
    assert_eq!(f.sentinels(), before);
    assert!(!f.temp.path().join("blocked/native").exists());
}
#[tokio::test]
async fn no_optin_keeps_legacy_default_even_in_a_feature_enabled_binary() {
    let f = Fixture::new();
    let before = f.sentinels();
    let result = output(f.command().args([
        "--offline",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-builtin-tools",
        "--provider=anthropic",
        "--api-key=fixture-only",
        "--print",
    ]))
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!f.native.exists());
    let after = f.sentinels();
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes));
    }
    let added = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .collect::<Vec<_>>();
    assert_eq!(
        added
            .iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .count(),
        2,
        "one primary and one progress JSONL expected: {added:?}"
    );
    assert!(added
        .iter()
        .all(|path| path.starts_with("settings sessions")));
}
