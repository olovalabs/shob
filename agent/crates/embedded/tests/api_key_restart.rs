use std::process::Command;

#[test]
fn provider_keys_survive_process_restart() {
    const PHASE: &str = "SHOB_TEST_AUTH_PHASE";
    if let Ok(phase) = std::env::var(PHASE) {
        match phase.as_str() {
            "save" => {
                embedded::save_api_key("commandcode", "test-api-key").unwrap();
                embedded::add_openai_compatible_provider(
                    "test-gateway",
                    "Test Gateway",
                    "https://example.com/v1",
                    "gateway-key",
                    &["model-a".into(), "model-b".into()],
                )
                .unwrap();
            }
            "read" => {
                let provider = embedded::providers()
                    .unwrap()
                    .into_iter()
                    .find(|provider| provider.id == "commandcode")
                    .unwrap();
                assert!(provider.ready);
                assert_eq!(provider.source.as_deref(), Some("auth.json"));
                let custom = embedded::providers()
                    .unwrap()
                    .into_iter()
                    .find(|p| p.id == "test-gateway")
                    .unwrap();
                assert!(custom.ready && custom.custom);
                assert_eq!(custom.name.as_deref(), Some("Test Gateway"));
                let models = embedded::chat_catalog().unwrap();
                for id in ["model-a", "model-b"] {
                    assert!(models
                        .iter()
                        .any(|m| m.provider == "test-gateway" && m.id == id));
                }
                assert!(embedded::add_openai_compatible_provider(
                    "test-gateway",
                    "Replacement",
                    "https://other.example/v1",
                    "replacement-key",
                    &["other".into()]
                )
                .is_err());
                assert!(embedded::add_openai_compatible_provider(
                    "openai",
                    "Replacement",
                    "https://other.example/v1",
                    "replacement-key",
                    &["other".into()]
                )
                .is_err());
                embedded::remove_custom_provider("test-gateway").unwrap();
                assert!(!embedded::providers()
                    .unwrap()
                    .iter()
                    .any(|p| p.id == "test-gateway"));
                assert!(!embedded::chat_catalog()
                    .unwrap()
                    .iter()
                    .any(|m| m.provider == "test-gateway"));
                embedded::disconnect_provider("commandcode").unwrap();
                assert!(
                    !embedded::providers()
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
            .args(["--exact", "provider_keys_survive_process_restart"])
            .env("SHOB_CODING_AGENT_DIR", config.path())
            .env_remove("COMMAND_CODE_API_KEY")
            .env_remove("COMMANDCODE_API_KEY")
            .env(PHASE, phase)
            .status()
            .unwrap();
        assert!(status.success(), "{phase} process failed");
    }
}
