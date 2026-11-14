use rpi_harness::session::jsonl::{encode_header, JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::{DefaultIdGenerator, SystemClock};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::{
    env, fs,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CHILD_ENV: &str = "RPI_DIAGNOSTICS_BROKEN_PIPE_CHILD";
const HISTORY_ENV: &str = "RPI_DIAGNOSTICS_BROKEN_PIPE_HISTORY";
const HOME_ENV: &str = "RPI_DIAGNOSTICS_BROKEN_PIPE_HOME";
const READY_ENV: &str = "RPI_DIAGNOSTICS_BROKEN_PIPE_READY";
const GO_ENV: &str = "RPI_DIAGNOSTICS_BROKEN_PIPE_GO";

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Also clean up on an assertion failure or a timeout while the child
        // is waiting for the parent's signal.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_file(path: &Path, deadline: Instant) {
    while !path.exists() {
        assert!(Instant::now() < deadline, "child did not become ready");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_child(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "diagnostics child did not exit");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn broken_stderr_does_not_change_streaming_load_result() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("private-home");
    let ready = dir.path().join("ready");
    let go = dir.path().join("go");
    fs::create_dir(&home).unwrap();
    let history = dir.path().join("synthetic-history.jsonl");
    let header = JsonlV4Header::new(
        "broken-pipe-test".into(),
        1,
        "/synthetic".into(),
        None,
        None,
        None,
    );
    fs::write(&history, encode_header(&header)).unwrap();

    let mut child = OwnedChild(
        Command::new(env::current_exe().unwrap())
            .args(["--exact", "child_load_with_broken_stderr", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env("RPI_SESSION_DIAGNOSTICS", "1")
            .env(HISTORY_ENV, &history)
            .env(HOME_ENV, &home)
            .env(READY_ENV, &ready)
            .env(GO_ENV, &go)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("HOMEDRIVE", "")
            .env("HOMEPATH", "")
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    // The child explicitly waits until this marker before loading. Closing the
    // only read end first makes the diagnostic write deterministically fail on
    // both Unix and Windows; this is intentionally not a null device.
    wait_for_file(&ready, Instant::now() + Duration::from_secs(10));
    drop(child.0.stderr.take());
    fs::write(&go, b"go").unwrap();

    let status = wait_for_child(&mut child.0);
    assert!(
        status.success(),
        "successful synthetic history load must survive a broken stderr pipe: {status}"
    );
}

#[tokio::test]
async fn child_load_with_broken_stderr() {
    if env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let history = env::var(HISTORY_ENV).unwrap();
    let home = env::var(HOME_ENV).unwrap();
    let ready = env::var(READY_ENV).unwrap();
    let go = env::var(GO_ENV).unwrap();
    fs::write(&ready, b"ready").unwrap();
    while !Path::new(&go).exists() {
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(env::var("HOME").unwrap(), home);
    assert_eq!(env::var("USERPROFILE").unwrap(), home);
    let fs: Arc<dyn FileSystem> =
        Arc::new(OsExecutionEnv::with_cwd(Path::new(&home).to_path_buf()));
    let loaded = JsonlSessionStorage::load(
        fs,
        &history,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await;
    assert!(loaded.is_ok(), "synthetic history should load successfully");
}
