//! Missing Web extensions must fail before stdin is read or a chat is opened.
use std::{process::Stdio, time::Duration};

#[tokio::test]
async fn missing_web_extension_reports_installation_without_waiting_for_stdin() {
    let root = tempfile::tempdir().unwrap();
    for flag in ["--web", "--web=init", "--web=help"] {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(root.path())
            .env("RPI_CODING_AGENT_DIR", root.path().join("agent"))
            .env("RPI_OFFLINE", "1")
            .env("ANTHROPIC_API_KEY", "fixture-only-no-model-calls")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL")
            .args([
                "--offline",
                "--no-session",
                "--package-only",
                "--provider",
                "anthropic",
                "--model",
                "claude-sonnet-4-5",
                flag,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Keep the pipe open: falling back to chat would wait for its EOF.
        let _held_input = child.stdin.take();
        let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
            .await
            .expect("--web must fail before reading stdin")
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("--web requires the rpi-web server extension"),
            "{error}"
        );
        assert!(
            error.contains("rpi install rpi-web --github pi-rust/rpi-package"),
            "{error}"
        );
        assert!(output.stdout.is_empty());
    }
}
