use std::process::Command;

#[test]
fn commandcode_key_survives_process_restart() {
    const PHASE: &str = "KATHAMO_TEST_AUTH_PHASE";
    if let Ok(phase) = std::env::var(PHASE) {
        match phase.as_str() {
            "save" => rpi_embedded::save_api_key("commandcode", "test-api-key").unwrap(),
            "read" => {
                let provider = rpi_embedded::providers()
                    .unwrap()
                    .into_iter()
                    .find(|provider| provider.id == "commandcode")
                    .unwrap();
                assert!(provider.ready);
                assert_eq!(provider.source.as_deref(), Some("auth.json"));
                rpi_embedded::disconnect_provider("commandcode").unwrap();
                assert!(
                    !rpi_embedded::providers()
                        .unwrap()
                        .into_iter()
                        .find(|provider| provider.id == "commandcode")
                        .unwrap()
                        .ready
                );
            }
            _ => panic!("unexpected test phase"),
        }
        return;
    }

    let config = tempfile::tempdir().unwrap();
    for phase in ["save", "read"] {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "commandcode_key_survives_process_restart"])
            .env("RPI_CODING_AGENT_DIR", config.path())
            .env_remove("COMMAND_CODE_API_KEY")
            .env_remove("COMMANDCODE_API_KEY")
            .env(PHASE, phase)
            .status()
            .unwrap();
        assert!(status.success(), "{phase} process failed");
    }
}
