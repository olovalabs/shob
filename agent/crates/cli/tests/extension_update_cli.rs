//! CLI-only regressions: no installed records, installers, or model traffic.
use std::{path::PathBuf, process::Stdio, time::Duration};

struct Cli {
    _root: tempfile::TempDir,
    home: PathBuf,
    agent: PathBuf,
    cwd: PathBuf,
}

impl Cli {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let agent = root.path().join("agent");
        let cwd = root.path().join("project");
        for dir in [&home, &agent, &cwd] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            home,
            agent,
            cwd,
        }
    }

    async fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
        command.env_clear();
        // Windows process/runtime support, not user configuration or credentials.
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("RPI_CODING_AGENT_DIR", &self.agent)
            .env("RPI_OFFLINE", "1")
            .env("CARGO_NET_OFFLINE", "true")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // A prompt fallback must not silently pass by observing stdin EOF.
        let _held_input = child.stdin.take();
        let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
            .await
            .expect("CLI dispatch must finish without reading stdin or contacting a model")
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    async fn both(&self, args: &[&str], code: i32) -> (String, String) {
        let mut new = vec!["extension", "update"];
        new.extend_from_slice(args);
        let mut old = vec!["package", "update"];
        old.extend_from_slice(args);
        let result = self.run(&new).await;
        assert_eq!(result, self.run(&old).await, "arguments: {args:?}");
        assert_eq!(result.0, code, "arguments: {args:?}, result: {result:?}");
        (result.1, result.2)
    }
}

#[tokio::test]
async fn extension_and_legacy_help_are_available_before_provider_startup() {
    let cli = Cli::new();
    let top = cli.run(&["--help"]).await;
    assert_eq!(top.0, 0);
    assert!(top.1.contains("extension update [name]"));
    for args in [
        vec!["extension"],
        vec!["extension", "--help"],
        vec!["extension", "-h"],
    ] {
        let result = cli.run(&args).await;
        assert_eq!(result.0, 0);
        assert!(result.1.contains("Usage: rpi extension <command>"));
        assert!(result.1.contains("update [name]"));
        assert!(result.2.is_empty());
    }
    let legacy = cli.run(&["package", "--help"]).await;
    assert_eq!(legacy.0, 0);
    assert!(legacy
        .1
        .contains("Legacy compatibility alias for `rpi extension update`"));
    for flag in ["--help", "-h"] {
        let (out, err) = cli.both(&[flag], 0).await;
        assert!(out.contains("Usage: rpi extension update [name]"));
        assert!(out.contains("--debug"));
        assert!(out.contains("--locked"));
        assert!(out.contains("rpi package update"));
        assert!(err.is_empty());
    }
    assert_eq!(std::fs::read_dir(&cli.agent).unwrap().count(), 0);
}

#[tokio::test]
async fn update_aliases_match_for_empty_registries_and_arguments() {
    let cli = Cli::new();
    // Missing registries and explicitly empty registries must behave identically.
    for explicit in [false, true] {
        if explicit {
            std::fs::create_dir_all(cli.cwd.join(".rpi")).unwrap();
            std::fs::write(cli.cwd.join(".rpi/packages.json"), r#"{"packages":[]}"#).unwrap();
            std::fs::write(cli.agent.join("native-packages.json"), "[]").unwrap();
        }
        for args in [
            vec![],
            vec!["--debug"],
            vec!["--locked"],
            vec!["--locked", "--debug"],
        ] {
            let (out, err) = cli.both(&args, 0).await;
            assert_eq!(out.trim(), "no project or global extensions to update");
            assert!(err.is_empty());
        }
        for args in [vec!["missing"], vec!["missing", "--debug", "--locked"]] {
            let (out, err) = cli.both(&args, 1).await;
            assert!(out.is_empty());
            assert!(err.contains("extension `missing` is not installed"));
        }
        for args in [
            vec!["one", "two"],
            vec!["--unknown"],
            vec!["--force"],
            vec!["--build"],
            vec!["--github", "owner/repo"],
            vec!["--offline"],
        ] {
            let (out, err) = cli.both(&args, 2).await;
            assert!(out.is_empty());
            assert!(err.contains("error:"));
        }
    }
    assert!(!cli.cwd.join(".rpi/packages").exists());
    assert!(!cli.agent.join("extensions").exists());
}

#[tokio::test]
async fn corrupt_registries_fail_identically_and_help_still_works() {
    let cli = Cli::new();
    std::fs::write(cli.agent.join("native-packages.json"), "not json").unwrap();
    let (_, err) = cli.both(&[], 1).await;
    assert!(err.contains("error:"));
    cli.both(&["--help"], 0).await;
    std::fs::create_dir_all(cli.cwd.join(".rpi")).unwrap();
    std::fs::write(cli.cwd.join(".rpi/packages.json"), "not json").unwrap();
    let (_, err) = cli.both(&[], 1).await;
    assert!(err.contains("error:"));
    cli.both(&["--help"], 0).await;
}

#[tokio::test]
async fn unsupported_extension_commands_are_usage_errors_not_model_prompts() {
    let cli = Cli::new();
    for subcommand in ["unknown", "link", "list", "unlink", "install"] {
        let result = cli.run(&["extension", subcommand]).await;
        assert_eq!(result.0, 2, "{result:?}");
        assert!(result
            .2
            .contains(&format!("unknown extension command `{subcommand}`")));
        assert!(result.1.contains("Usage: rpi extension <command>"));
    }
    assert_eq!(std::fs::read_dir(&cli.agent).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&cli.cwd).unwrap().count(), 0);
}
