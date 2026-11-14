//! Build only the real workspace cdylib, never tests. Each fixture owns a fresh
//! DLL copy, avoiding cross-process DLL locking even with a shared Cargo cache.
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

pub struct Fixture {
    // Owned rather than a leaked static: removed after native hosts have exited.
    _directory: tempfile::TempDir,
    pub library: PathBuf,
    pub source_digest: String,
}
fn hash_tree(path: &Path, hash: &mut Sha256) {
    if path.is_file() {
        hash.update(std::fs::read(path).unwrap());
    } else {
        let mut paths: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        for path in paths {
            hash_tree(&path, hash);
        }
    }
}
impl Fixture {
    pub async fn build() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let mut hash = Sha256::new();
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "examples/plugin-stub/Cargo.toml",
            "examples/plugin-stub/src",
            "crates/rpi-plugin-sdk/Cargo.toml",
            "crates/rpi-plugin-sdk/src",
        ] {
            hash_tree(&workspace.join(file), &mut hash);
        }
        let source_digest = format!("{:x}", hash.finalize());
        let directory = if let Some(cache) = std::env::var_os("CARGO_TARGET_DIR") {
            std::fs::create_dir_all(&cache).unwrap();
            tempfile::Builder::new()
                .prefix("resource-fixture-")
                .tempdir_in(cache)
                .unwrap()
        } else {
            tempfile::Builder::new()
                .prefix("resource-fixture-")
                .tempdir()
                .unwrap()
        };
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.path().join("target"));
        let build = std::env::var_os("CARGO_BUILD_BUILD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.path().join("build"));
        let log_path = directory.path().join("build.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = tokio::process::Command::new(
            std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .current_dir(workspace)
        .args([
            "build",
            "--locked",
            "--offline",
            "--quiet",
            "-p",
            "plugin-stub",
            "--lib",
            "--target-dir",
        ])
        .arg(&target)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_BUILD_DIR", build)
        .env("CARGO_INCREMENTAL", "0")
        .env("RPI_RESOURCE_FIXTURE_SOURCE_SHA256", &source_digest)
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_PROFILE_DEV_DEBUG", "0")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if start.elapsed() > Duration::from_secs(120)
                || std::fs::metadata(&log_path).unwrap().len() > 2 * 1024 * 1024
            {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                break None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let mut output = String::new();
        std::fs::File::open(log_path)
            .unwrap()
            .take(2 * 1024 * 1024)
            .read_to_string(&mut output)
            .unwrap();
        assert!(
            status.is_some_and(|s| s.success()),
            "bounded fixture build failed/timed out: {output}"
        );
        let library = target.join("debug").join(format!(
            "{}plugin_stub.{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_EXTENSION
        ));
        assert!(
            library.is_file(),
            "isolated fixture build did not produce {}: {output}",
            library.display()
        );
        // A successful source-digest-stamped build is mandatory; never fall
        // back to an old artifact. Hosts load only the fixture-owned copy.
        let owned_library = directory.path().join(library.file_name().unwrap());
        std::fs::copy(&library, &owned_library).unwrap();
        Self {
            _directory: directory,
            library: owned_library,
            source_digest,
        }
    }
    pub fn assert_registrations(&self, path: &Path) -> String {
        let log = std::fs::read_to_string(path).unwrap();
        assert!(!log.is_empty());
        assert!(
            log.lines()
                .all(|line| line == format!("register {}", self.source_digest)),
            "not built from current source: {log}"
        );
        log
    }
}
