use super::*;
#[tokio::test]
async fn build_revalidates_post_prepare_changes_and_keeps_native_root_frozen() {
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("old");
    let _env = crate::config::test_support::SessionDirEnv::new(Some(&old));
    let _restore = Environment::new();
    std::env::remove_var("RPI_FORCE_TUI");
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    for change in 0..11 {
        let mut arguments = args();
        let prepared = prepare_extensions(&arguments, temp.path(), false).unwrap();
        match change {
            0 => arguments.experimental_native_store = Some("changed native".into()),
            1 => arguments.session_dir = Some(old.clone()),
            2 => arguments.continue_session = true,
            3 => arguments.session_id = Some("legacy".into()),
            4 => arguments.fork = Some("legacy".into()),
            5 => arguments.no_session = true,
            6 => arguments.mode = crate::args::Mode::Rpc,
            7 => {
                arguments
                    .unknown_flags
                    .insert("server".into(), false.into());
            }
            8 => {
                arguments.unknown_flags.insert("web".into(), true.into());
            }
            9 => std::env::set_var("RPI_FORCE_TUI", "1"),
            _ => arguments.print = false,
        }
        assert!(
            matches!(
                build_prepared(&resolved(), &arguments, temp.path(), false, prepared).await,
                Err(BuildError::NativeStorePolicy(_))
            ),
            "accepted change {change}"
        );
        std::env::remove_var("RPI_FORCE_TUI");
        assert!(!temp.path().join("native").exists());
        assert!(!temp.path().join("changed native").exists());
        assert!(!old.exists());
    }
    let arguments = args();
    let prepared = prepare_extensions(&arguments, temp.path(), false).unwrap();
    let changed = temp.path().join("changed old sessions");
    std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", &changed);
    std::fs::write(
        agent.join("settings.json"),
        serde_json::json!({"sessionDir":changed}).to_string(),
    )
    .unwrap();
    let (harness, _events, context) =
        build_prepared(&resolved(), &arguments, temp.path(), false, prepared)
            .await
            .unwrap();
    assert!(context.session_dir().ends_with("native"));
    assert!(!changed.exists());
    assert!(!old.exists());
    let host = context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_host();
    host.append_entry(serde_json::json!({"customType":"native fixture","data":null}))
        .await
        .unwrap();
    let custom = harness
        .session()
        .find_entries(&rpi_harness::session::types::EntryQuery {
            custom_type: Some("native fixture".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(custom.len(), 1);
    assert!(
        matches!(&custom[0],rpi_harness::session::types::Entry::Custom(e)if e.data==Some(serde_json::Value::Null))
    );
}
