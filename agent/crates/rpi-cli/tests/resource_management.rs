//! Management-only process regressions. Cargo is a fixture executable returning
//! local metadata/artifact records; it never builds a library or loads code.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

fn put(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

struct Cli {
    directory: Option<tempfile::TempDir>,
    root: PathBuf,
    artifact: String,
    calls: std::cell::Cell<usize>,
}
impl Drop for Cli {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(directory) = self.directory.take() {
                eprintln!(
                    "management failure evidence: {}",
                    directory.keep().display()
                );
            }
        }
    }
}
impl Cli {
    fn new() -> Self {
        let directory = if let Some(path) = std::env::var_os("RPI_RESOURCE_REVIEW_EVIDENCE_DIR") {
            std::fs::create_dir_all(&path).unwrap();
            tempfile::tempdir_in(path).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let root = directory.path().to_path_buf();
        let artifact = format!("fixture.{}", std::env::consts::DLL_EXTENSION);
        put(
            root.join("artifact").join(&artifact),
            b"fixture artifact, never executable",
        );
        put(root.join("source/Cargo.toml"), b"fixture metadata only");
        let metadata = json!({"packages":[{"name":"fixture","version":"1.0","manifest_path":root.join("source/Cargo.toml"),"targets":[{"name":"fixture","crate_types":["cdylib"]}]}]});
        let built = json!({"reason":"compiler-artifact","target":{"name":"fixture"},"filenames":[root.join("artifact").join(&artifact)]});
        put(root.join("metadata.json"), metadata.to_string());
        put(root.join("built.json"), built.to_string());
        put(
            root.join("cargo.rs"),
            r#"
use std::{env,fs,io::Write};
fn main() {
    let root = env::var_os("FIXTURE_CARGO_ROOT").unwrap();
    let root = std::path::Path::new(&root);
    let op = env::args().nth(1).unwrap();
    let mut log = fs::OpenOptions::new().create(true).append(true).open(root.join("cargo.log")).unwrap();
    writeln!(log,"{op}").unwrap();
    let file = match op.as_str() { "metadata" => "metadata.json", "build" => "built.json", _ => panic!("unexpected cargo operation") };
    print!("{}",fs::read_to_string(root.join(file)).unwrap());
}
"#,
        );
        std::fs::create_dir(root.join("bin")).unwrap();
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--crate-name", "fixture_cargo", "--edition", "2021"])
            .arg(root.join("cargo.rs"))
            .arg("-o")
            .arg(
                root.join("bin")
                    .join(format!("cargo{}", std::env::consts::EXE_SUFFIX)),
            )
            .output()
            .unwrap();
        put(root.join("rustc.stderr"), &output.stderr);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::create_dir(root.join("project")).unwrap();
        Self {
            directory: Some(directory),
            root,
            artifact,
            calls: std::cell::Cell::new(0),
        }
    }
    fn native(&self) -> PathBuf {
        self.root.join("project/.rpi")
    }
    async fn run(&self, args: &[&str]) -> Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
        command.env_clear();
        for key in [
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_BUILD_DIR",
            "CARGO_BUILD_JOBS",
            "CARGO_INCREMENTAL",
            "CARGO_PROFILE_DEV_DEBUG",
            "CARGO_PROFILE_TEST_DEBUG",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            command
                .current_dir(self.root.join("project"))
                .env("HOME", self.root.join("home"))
                .env("USERPROFILE", self.root.join("home"))
                .env("RPI_CODING_AGENT_DIR", self.root.join("agent"))
                .env("FIXTURE_CARGO_ROOT", &self.root)
                .env("PATH", self.root.join("bin"))
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("management CLI hung")
        .unwrap();
        let call = self.calls.get();
        self.calls.set(call + 1);
        put(self.root.join(format!("{call}.args")), args.join("\n"));
        put(self.root.join(format!("{call}.stdout")), &output.stdout);
        put(self.root.join(format!("{call}.stderr")), &output.stderr);
        output
    }
    async fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args).await;
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    async fn install_package(&self) {
        put(
            self.root.join("bundle/extensions").join(&self.artifact),
            b"managed bytes",
        );
        self.ok(&[
            "resource",
            "install",
            "package",
            "demo",
            "--scope",
            "project",
            "--path",
            self.root.join("bundle").to_str().unwrap(),
        ])
        .await;
    }
    async fn alias(&self, link: &Path, target: &Path) {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        #[cfg(windows)]
        {
            let out = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link.to_string_lossy().replace('/', "\\"))
                .arg(target.to_string_lossy().replace('/', "\\"))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    fn protected_bytes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        [
            PathBuf::from("packages/demo/extensions").join(&self.artifact),
            "packages.json".into(),
            "resource-receipts.json".into(),
        ]
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(self.native().join(&p)).unwrap();
            (p, bytes)
        })
        .collect()
    }
    fn assert_protected(&self, before: &[(PathBuf, Vec<u8>)]) {
        for (path, bytes) in before {
            assert_eq!(
                &std::fs::read(self.native().join(path)).unwrap(),
                bytes,
                "protected bytes changed: {}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn legacy_package_alias_cannot_replace_managed_artifact_or_metadata() {
    let cli = Cli::new();
    cli.install_package().await;
    let before = cli.protected_bytes();
    let direct = cli
        .run(&["package", "link", "demo", "--force", "--path", "../source"])
        .await;
    assert!(!direct.status.success());
    cli.assert_protected(&before);
    #[cfg(windows)]
    {
        // Win32 resolves the trailing-dot spelling to the managed directory.
        // Reject the addressed identity before metadata/build or any writes.
        let trailing_dot = cli
            .run(&["package", "link", "demo.", "--force", "--path", "../source"])
            .await;
        assert!(!trailing_dot.status.success());
        cli.assert_protected(&before);
        assert!(!cli.root.join("cargo.log").exists());
    }
    cli.alias(
        &cli.native().join("packages/alias"),
        &cli.native().join("packages/demo"),
    )
    .await;
    let alias = cli
        .run(&["package", "link", "alias", "--force", "--path", "../source"])
        .await;
    cli.assert_protected(&before);
    assert!(!alias.status.success(), "alias link unexpectedly succeeded");
    assert!(String::from_utf8_lossy(&alias.stderr).contains("links/reparse"));
    assert!(
        !cli.root.join("cargo.log").exists(),
        "aliased directory must fail before Cargo"
    );
    for args in [
        vec!["package", "unlink", "alias"],
        vec!["package", "update", "alias"],
    ] {
        // Make the alias a registered legacy candidate too, so update/unlink
        // exercise their shared authority rather than a missing-name fallback.
        let mut registry: Value =
            serde_json::from_slice(&std::fs::read(cli.native().join("packages.json")).unwrap())
                .unwrap();
        if registry["packages"].as_array().unwrap().len() == 1 {
            let mut alias_record = registry["packages"][0].clone();
            alias_record["name"] = json!("alias");
            alias_record["source"] = json!(cli.root.join("source"));
            alias_record["cargo_package"] = json!("fixture");
            registry["packages"]
                .as_array_mut()
                .unwrap()
                .push(alias_record);
            put(cli.native().join("packages.json"), registry.to_string());
        }
        let before = cli.protected_bytes();
        assert!(!cli.run(&args).await.status.success());
        cli.assert_protected(&before);
        assert!(!cli.root.join("cargo.log").exists());
    }
}

#[tokio::test]
async fn legacy_package_extensions_alias_is_rejected_before_build_or_removal() {
    let cli = Cli::new();
    cli.install_package().await;
    cli.alias(
        &cli.native().join("packages/alias/extensions"),
        &cli.native().join("packages/demo/extensions"),
    )
    .await;
    let before = cli.protected_bytes();
    for args in [
        vec!["package", "link", "alias", "--force", "--path", "../source"],
        vec!["package", "unlink", "alias"],
    ] {
        let output = cli.run(&args).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("links/reparse"));
        cli.assert_protected(&before);
    }
    assert!(!cli.root.join("cargo.log").exists());
}

#[tokio::test]
async fn preexisting_package_temp_hardlink_is_not_truncated_removed_or_activated() {
    let cli = Cli::new();
    cli.install_package().await;
    cli.ok(&["package", "link", "plain", "--path", "../source"])
        .await;
    let before = cli.protected_bytes();
    let target = cli
        .native()
        .join("packages/plain/extensions")
        .join(&cli.artifact);
    let old_target = std::fs::read(&target).unwrap();
    let temp = target.with_file_name(format!(".{}.tmp", cli.artifact));
    let managed = cli
        .native()
        .join("packages/demo/extensions")
        .join(&cli.artifact);
    std::fs::hard_link(&managed, &temp).unwrap();
    let output = cli
        .run(&["package", "link", "plain", "--force", "--path", "../source"])
        .await;
    assert!(!output.status.success());
    cli.assert_protected(&before);
    assert_eq!(
        std::fs::read(&temp).unwrap(),
        b"managed bytes",
        "pre-existing temp must not be deleted"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        old_target,
        "refusal must not remove the current artifact"
    );
}

#[tokio::test]
async fn legacy_package_artifact_alias_is_checked_and_versioned_force_preserves_legacy_artifacts() {
    let cli = Cli::new();
    cli.install_package().await;
    let managed = cli
        .native()
        .join("packages/demo/extensions")
        .join(&cli.artifact);
    let target = cli
        .native()
        .join("packages/plain/extensions")
        .join(&cli.artifact);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&managed, &target).unwrap();
    #[cfg(windows)]
    cli.alias(&target, &cli.native().join("packages/demo/extensions"))
        .await;
    let before = cli.protected_bytes();
    let output = cli
        .run(&["package", "link", "plain", "--force", "--path", "../source"])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("links/reparse"));
    cli.assert_protected(&before);
    #[cfg(unix)]
    std::fs::remove_file(&target).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(&target).unwrap();
    // A hardlinked final artifact is safe to replace by unlink + rename; never
    // truncate it in place. This is not an inode-level sandbox claim.
    std::fs::hard_link(&managed, &target).unwrap();
    cli.ok(&["package", "link", "plain", "--force", "--path", "../source"])
        .await;
    assert_eq!(std::fs::read(&managed).unwrap(), b"managed bytes");
    // The legacy hardlinked artifact is retained rather than replaced. The
    // active registry entry points at the newly published versioned artifact.
    assert_eq!(std::fs::read(&target).unwrap(), b"managed bytes");
    let registry: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cli.root.join("project/.rpi/packages.json")).unwrap(),
    )
    .unwrap();
    let active = registry["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "plain")
        .and_then(|package| package["artifacts"][0].as_str())
        .unwrap();
    let active_path = target.parent().unwrap().join(active);
    assert_ne!(active_path, target);
    assert_eq!(
        std::fs::read(active_path).unwrap(),
        b"fixture artifact, never executable"
    );
    cli.ok(&["package", "link", "plain", "--force", "--path", "../source"])
        .await;
    cli.ok(&["package", "unlink", "plain"]).await;
    assert!(!target.exists());
    assert_eq!(std::fs::read(&managed).unwrap(), b"managed bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn preexisting_package_temp_symlink_is_refused_without_changing_managed_bytes() {
    let cli = Cli::new();
    cli.install_package().await;
    let temp = cli
        .native()
        .join("packages/plain/extensions")
        .join(format!(".{}.tmp", cli.artifact));
    std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(
        cli.native()
            .join("packages/demo/extensions")
            .join(&cli.artifact),
        &temp,
    )
    .unwrap();
    let before = cli.protected_bytes();
    let output = cli
        .run(&["package", "link", "plain", "--force", "--path", "../source"])
        .await;
    assert!(!output.status.success());
    cli.assert_protected(&before);
    assert!(std::fs::symlink_metadata(temp)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[tokio::test]
async fn safe_legacy_package_names_allow_list_inspect_and_unrelated_install() {
    let cli = Cli::new();
    for name in ["_legacy", ".tools"] {
        cli.ok(&["package", "link", name, "--path", "../source"])
            .await;
    }
    let registry = std::fs::read(cli.native().join("packages.json")).unwrap();
    cli.ok(&["package", "list"]).await;
    let list = cli.ok(&["resource", "list", "--scope", "project"]).await;
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    for name in ["_legacy", ".tools"] {
        assert!(list["data"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == name && r["ownership"] == "legacy-native"));
        cli.ok(&["resource", "inspect", "package", name, "--scope", "project"])
            .await;
        let denied = cli
            .run(&[
                "resource",
                "install",
                "package",
                name,
                "--scope",
                "project",
                "--path",
                "../source",
            ])
            .await;
        assert!(!denied.status.success(), "new managed grammar widened");
    }
    put(cli.root.join("skill/SKILL.md"), b"body");
    cli.ok(&[
        "resource",
        "install",
        "skill",
        "unrelated",
        "--scope",
        "project",
        "--path",
        "../skill",
    ])
    .await;
    assert_eq!(
        registry,
        std::fs::read(cli.native().join("packages.json")).unwrap()
    );
    cli.install_package().await;
    let registry: Value =
        serde_json::from_slice(&std::fs::read(cli.native().join("packages.json")).unwrap())
            .unwrap();
    for name in ["_legacy", ".tools", "demo"] {
        assert!(registry["packages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == name));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn snapshots_preserve_executable_modes_and_strip_special_bits_on_every_copy() {
    use std::os::unix::fs::PermissionsExt;
    let cli = Cli::new();
    for (kind, source, relative) in [
        ("skill", "skill", "scripts/check.sh"),
        ("package", "bundle", "skills/demo/scripts/check.sh"),
    ] {
        let source = cli.root.join(source);
        put(
            source.join(if kind == "skill" {
                "SKILL.md"
            } else {
                "skills/demo/SKILL.md"
            }),
            b"body",
        );
        let script = source.join(relative);
        let target = cli
            .native()
            .join(if kind == "skill" {
                "skills/demo"
            } else {
                "packages/demo"
            })
            .join(relative);
        for (op, mode) in [
            ("install", 0o755),
            ("update", 0o7754),
            ("reinstall", 0o4755),
        ] {
            put(&script, b"#!/bin/sh\nprintf 'helper-ran\\n'\n");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                std::fs::metadata(&script).unwrap().permissions().mode() & 0o7777,
                mode
            );
            cli.ok(&[
                "resource",
                op,
                kind,
                "demo",
                "--scope",
                "project",
                "--path",
                source.to_str().unwrap(),
            ])
            .await;
            let actual = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
            assert_eq!(
                actual,
                mode & 0o777,
                "{kind} {op}: executable/rwx bits lost or special bits propagated"
            );
            let out = Command::new(&target)
                .env_clear()
                .output()
                .expect("copied helper must be directly executable");
            assert!(out.status.success());
            assert_eq!(out.stdout, b"helper-ran\n");
        }
    }
}
